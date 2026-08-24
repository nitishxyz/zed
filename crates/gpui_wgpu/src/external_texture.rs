use anyhow::{Context as _, Result, anyhow, bail};
use ash::{ext, khr, vk};
use gpui::{DmabufTextureDescriptor, ExternalTexture};
use std::{
    os::fd::{FromRawFd, IntoRawFd},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use wgpu::hal::{self, api::Vulkan};

const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const DRM_FORMAT_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
const DRM_FORMAT_XBGR8888: u32 = u32::from_le_bytes(*b"XB24");
const MAX_PENDING_DMABUF_RETIREMENTS: usize = 16;

pub(crate) struct RendererOwnedTexture {
    _texture: wgpu::Texture,
    pub(crate) view: wgpu::TextureView,
}

struct ImportedTexture {
    texture: wgpu::Texture,
}

pub(crate) struct DmabufRetirement {
    pending: AtomicUsize,
    max_pending: usize,
}

impl DmabufRetirement {
    pub(crate) fn new() -> Self {
        Self {
            pending: AtomicUsize::new(0),
            max_pending: MAX_PENDING_DMABUF_RETIREMENTS,
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<DmabufRetirementPermit> {
        self.pending
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |pending| {
                (pending < self.max_pending).then_some(pending + 1)
            })
            .ok()?;
        Some(DmabufRetirementPermit {
            retirement: self.clone(),
        })
    }

    #[cfg(test)]
    fn with_capacity(max_pending: usize) -> Self {
        Self {
            pending: AtomicUsize::new(0),
            max_pending,
        }
    }
}

struct DmabufRetirementPermit {
    retirement: Arc<DmabufRetirement>,
}

impl Drop for DmabufRetirementPermit {
    fn drop(&mut self) {
        let previous = self.retirement.pending.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(previous > 0);
    }
}

pub(crate) fn create_vulkan_device(
    adapter: &wgpu::Adapter,
    descriptor: &wgpu::DeviceDescriptor<'_>,
) -> Result<Option<(wgpu::Device, wgpu::Queue)>> {
    if adapter.get_info().backend != wgpu::Backend::Vulkan {
        return Ok(None);
    }

    // SAFETY: The HAL adapter and device are kept within wgpu's ownership model. The callback
    // only adds an extension advertised by this physical device and does not alter features.
    unsafe {
        let hal_adapter = adapter
            .as_hal::<Vulkan>()
            .ok_or_else(|| anyhow!("Vulkan adapter did not expose a wgpu-hal adapter"))?;
        for extension in [
            khr::external_memory_fd::NAME,
            ext::external_memory_dma_buf::NAME,
            ext::image_drm_format_modifier::NAME,
        ] {
            if !hal_adapter
                .physical_device_capabilities()
                .supports_extension(extension)
            {
                return Ok(None);
            }
        }

        let hal_device = hal_adapter
            .open_with_callback(
                descriptor.required_features,
                &descriptor.required_limits,
                &descriptor.memory_hints,
                Some(Box::new(|arguments| {
                    if !arguments
                        .extensions
                        .contains(&ext::image_drm_format_modifier::NAME)
                    {
                        arguments
                            .extensions
                            .push(ext::image_drm_format_modifier::NAME);
                    }
                })),
            )
            .map_err(|error| anyhow!("Failed to open Vulkan device for DMA-BUF import: {error}"))?;

        adapter
            .create_device_from_hal::<Vulkan>(hal_device, descriptor)
            .map(Some)
            .map_err(|error| anyhow!("Failed to create wgpu Vulkan device: {error}"))
    }
}

pub(crate) fn copy_dmabuf_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    retirement: &Arc<DmabufRetirement>,
    descriptor: DmabufTextureDescriptor,
) -> Result<ExternalTexture> {
    let retirement_permit = match retirement.try_acquire() {
        Some(permit) => permit,
        None => {
            device
                .poll(wgpu::PollType::Poll)
                .context("Failed to poll completed DMA-BUF copies")?;
            retirement.try_acquire().ok_or_else(|| {
                anyhow!(
                    "too many DMA-BUF textures are awaiting GPU copy completion (maximum {})",
                    retirement.max_pending
                )
            })?
        }
    };
    let format = texture_format_for_drm_fourcc(descriptor.drm_format)?;
    validate_descriptor(&descriptor)?;
    let size = wgpu::Extent3d {
        width: descriptor.width,
        height: descriptor.height,
        depth_or_array_layers: 1,
    };
    let imported_texture = import_dmabuf_texture(device, descriptor, size, format)?;
    let owned_texture = device.create_texture(&owned_texture_descriptor(size, format));
    let view = owned_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("gpui_dmabuf_copy"),
    });
    encoder.copy_texture_to_texture(
        imported_texture.texture.as_image_copy(),
        owned_texture.as_image_copy(),
        size,
    );
    queue.submit([encoder.finish()]);
    queue.on_submitted_work_done(move || {
        imported_texture.texture.destroy();
        drop(imported_texture);
        drop(retirement_permit);
    });

    Ok(ExternalTexture::new(Arc::new(RendererOwnedTexture {
        _texture: owned_texture,
        view,
    })))
}

fn import_dmabuf_texture(
    device: &wgpu::Device,
    descriptor: DmabufTextureDescriptor,
    size: wgpu::Extent3d,
    format: wgpu::TextureFormat,
) -> Result<ImportedTexture> {
    // SAFETY: Every Vulkan object is created from wgpu's own logical device. The imported file
    // descriptor is transferred to Vulkan only after successful allocation, and the HAL texture
    // takes ownership of both the image and dedicated memory. Descriptor validation ensures all
    // Vulkan slices and dimensions are valid. The application relies on Linux implicit DMA-BUF
    // synchronization; importing explicit semaphores is a follow-up.
    unsafe {
        let hal_device = device
            .as_hal::<Vulkan>()
            .ok_or_else(|| anyhow!("DMA-BUF import requires the Vulkan wgpu backend"))?;
        let raw_device = hal_device.raw_device();
        let raw_instance = hal_device.shared_instance().raw_instance();

        for extension in [
            khr::external_memory_fd::NAME,
            ext::external_memory_dma_buf::NAME,
            ext::image_drm_format_modifier::NAME,
        ] {
            if !hal_device.enabled_device_extensions().contains(&extension) {
                bail!(
                    "Vulkan device did not enable required DMA-BUF extension {}",
                    extension.to_string_lossy()
                );
            }
        }

        let plane_layouts = descriptor
            .planes
            .iter()
            .map(|plane| {
                vk::SubresourceLayout::default()
                    .offset(plane.offset)
                    .size(plane.size)
                    .row_pitch(u64::from(plane.stride))
            })
            .collect::<Vec<_>>();
        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(descriptor.modifier)
            .plane_layouts(&plane_layouts);
        let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_info = vk::ImageCreateInfo::default()
            .push_next(&mut external_info)
            .push_next(&mut modifier_info)
            .image_type(vk::ImageType::TYPE_2D)
            .format(vulkan_format(format))
            .extent(vk::Extent3D {
                width: descriptor.width,
                height: descriptor.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        let image = raw_device
            .create_image(&image_info, None)
            .context("Failed to create Vulkan image for DMA-BUF")?;
        let memory_requirements = raw_device.get_image_memory_requirements(image);
        let first_plane = descriptor
            .planes
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("DMA-BUF texture has no planes"))?;
        let raw_fd = first_plane.fd.into_raw_fd();
        let external_memory = khr::external_memory_fd::Device::new(raw_instance, raw_device);
        let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
        if let Err(error) = external_memory.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            raw_fd,
            &mut fd_properties,
        ) {
            drop(std::os::fd::OwnedFd::from_raw_fd(raw_fd));
            raw_device.destroy_image(image, None);
            return Err(anyhow!(error).context("Failed to query DMA-BUF memory properties"));
        }

        let memory_type_bits =
            memory_requirements.memory_type_bits & fd_properties.memory_type_bits;
        let memory_type_index = match (0..32).find(|index| memory_type_bits & (1 << index) != 0) {
            Some(index) => index,
            None => {
                drop(std::os::fd::OwnedFd::from_raw_fd(raw_fd));
                raw_device.destroy_image(image, None);
                bail!("DMA-BUF has no Vulkan-compatible memory type");
            }
        };

        let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(raw_fd);
        let allocation_info = vk::MemoryAllocateInfo::default()
            .push_next(&mut dedicated_info)
            .push_next(&mut import_info)
            .allocation_size(memory_requirements.size)
            .memory_type_index(memory_type_index);
        let memory = match raw_device.allocate_memory(&allocation_info, None) {
            Ok(memory) => memory,
            Err(error) => {
                drop(std::os::fd::OwnedFd::from_raw_fd(raw_fd));
                raw_device.destroy_image(image, None);
                return Err(anyhow!(error).context("Failed to import DMA-BUF Vulkan memory"));
            }
        };

        if let Err(error) = raw_device.bind_image_memory(image, memory, 0) {
            raw_device.free_memory(memory, None);
            raw_device.destroy_image(image, None);
            return Err(anyhow!(error).context("Failed to bind imported DMA-BUF memory"));
        }

        let hal_descriptor = hal::TextureDescriptor {
            label: Some("gpui_imported_dmabuf_texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::wgt::TextureUses::COPY_SRC,
            memory_flags: hal::MemoryFlags::empty(),
            view_formats: Vec::new(),
        };
        let hal_texture = hal_device.texture_from_raw(
            image,
            &hal_descriptor,
            None,
            hal::vulkan::TextureMemory::Dedicated(memory),
        );
        let wgpu_descriptor = wgpu::TextureDescriptor {
            label: Some("gpui_imported_dmabuf_texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        };
        let texture = device.create_texture_from_hal::<Vulkan>(hal_texture, &wgpu_descriptor);

        Ok(ImportedTexture { texture })
    }
}

fn owned_texture_descriptor(
    size: wgpu::Extent3d,
    format: wgpu::TextureFormat,
) -> wgpu::TextureDescriptor<'static> {
    wgpu::TextureDescriptor {
        label: Some("gpui_owned_external_texture"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    }
}

fn validate_descriptor(descriptor: &DmabufTextureDescriptor) -> Result<()> {
    if descriptor.width == 0 || descriptor.height == 0 {
        bail!("DMA-BUF texture dimensions must be non-zero");
    }
    if descriptor.planes.is_empty() || descriptor.planes.len() > 4 {
        bail!("DMA-BUF texture must have between one and four planes");
    }
    if descriptor
        .planes
        .iter()
        .any(|plane| plane.stride == 0 || plane.size == 0)
    {
        bail!("DMA-BUF plane stride and size must be non-zero");
    }
    Ok(())
}

fn texture_format_for_drm_fourcc(fourcc: u32) -> Result<wgpu::TextureFormat> {
    match fourcc {
        DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 => Ok(wgpu::TextureFormat::Bgra8Unorm),
        DRM_FORMAT_ABGR8888 | DRM_FORMAT_XBGR8888 => Ok(wgpu::TextureFormat::Rgba8Unorm),
        _ => bail!("unsupported DMA-BUF DRM format {fourcc:#010x}"),
    }
}

fn vulkan_format(format: wgpu::TextureFormat) -> vk::Format {
    match format {
        wgpu::TextureFormat::Bgra8Unorm => vk::Format::B8G8R8A8_UNORM,
        wgpu::TextureFormat::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
        _ => unreachable!("validated DMA-BUF texture format"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_drm_fourcc_formats() {
        assert_eq!(
            texture_format_for_drm_fourcc(DRM_FORMAT_ARGB8888).unwrap(),
            wgpu::TextureFormat::Bgra8Unorm
        );
        assert_eq!(
            texture_format_for_drm_fourcc(DRM_FORMAT_ABGR8888).unwrap(),
            wgpu::TextureFormat::Rgba8Unorm
        );
        assert!(texture_format_for_drm_fourcc(0).is_err());
    }

    #[test]
    fn owned_texture_supports_copy_out_and_sampling() {
        let size = wgpu::Extent3d {
            width: 2800,
            height: 1756,
            depth_or_array_layers: 1,
        };
        let descriptor = owned_texture_descriptor(size, wgpu::TextureFormat::Bgra8Unorm);

        assert_eq!(descriptor.size, size);
        assert_eq!(descriptor.format, wgpu::TextureFormat::Bgra8Unorm);
        assert_eq!(
            descriptor.usage,
            wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING
        );
    }

    #[test]
    fn dmabuf_retirement_is_bounded_and_releases_capacity() -> Result<()> {
        let retirement = Arc::new(DmabufRetirement::with_capacity(2));
        let first = retirement
            .try_acquire()
            .ok_or_else(|| anyhow!("first retirement permit was unavailable"))?;
        let second = retirement
            .try_acquire()
            .ok_or_else(|| anyhow!("second retirement permit was unavailable"))?;

        assert!(retirement.try_acquire().is_none());
        drop(first);
        assert!(retirement.try_acquire().is_some());
        drop(second);
        Ok(())
    }
}
