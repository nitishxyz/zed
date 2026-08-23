use std::{any::Any, fmt, os::fd::OwnedFd, sync::Arc};

/// One plane of a Linux DMA-BUF texture.
#[derive(Debug)]
pub struct DmabufTexturePlane {
    /// An owned DMA-BUF file descriptor.
    pub fd: OwnedFd,
    /// The distance in bytes between adjacent rows.
    pub stride: u32,
    /// The byte offset of this plane within the DMA-BUF.
    pub offset: u64,
    /// The size in bytes of this plane.
    pub size: u64,
}

/// Describes a Linux DMA-BUF texture to import into the renderer.
#[derive(Debug)]
pub struct DmabufTextureDescriptor {
    /// The texture width in pixels.
    pub width: u32,
    /// The texture height in pixels.
    pub height: u32,
    /// The DRM fourcc pixel format.
    pub drm_format: u32,
    /// The DRM format modifier.
    pub modifier: u64,
    /// The DMA-BUF planes, in DRM plane order.
    pub planes: Vec<DmabufTexturePlane>,
}

/// An opaque handle to a renderer-owned external texture.
#[derive(Clone)]
pub struct ExternalTexture(Arc<dyn Any + Send + Sync>);

impl ExternalTexture {
    /// Creates an external texture handle from renderer-owned state.
    #[doc(hidden)]
    pub fn new(texture: Arc<dyn Any + Send + Sync>) -> Self {
        Self(texture)
    }

    /// Returns the renderer-owned state stored in this handle.
    #[doc(hidden)]
    pub fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self.0.as_ref()
    }
}

impl fmt::Debug for ExternalTexture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalTexture")
            .finish_non_exhaustive()
    }
}
