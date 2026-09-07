use std::{
    cell::RefCell,
    fs::File,
    io::{self, Cursor, ErrorKind, Write},
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    rc::{Rc, Weak},
    time::Duration,
};

use calloop::{
    LoopHandle, PostAction, RegistrationToken,
    timer::{TimeoutAction, Timer},
};
use collections::HashMap;
use filedescriptor::Pipe;
use strum::IntoEnumIterator;
use uuid::Uuid;
use wayland_client::{Connection, protocol::wl_data_offer::WlDataOffer};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1;

use crate::linux::{
    WaylandClientStatePtr,
    platform::{PIPE_READ_TIMEOUT, read_fd_with_timeout},
};
use gpui::{ClipboardEntry, ClipboardItem, DeferredClipboardImageError, Image, ImageFormat, hash};

/// Text mime types that we'll offer to other programs.
pub(crate) const TEXT_MIME_TYPES: [&str; 3] =
    ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
pub(crate) const FILE_LIST_MIME_TYPE: &str = "text/uri-list";

/// Text mime types that we'll accept from other programs.
pub(crate) const ALLOWED_TEXT_MIME_TYPES: [&str; 2] = ["text/plain;charset=utf-8", "UTF8_STRING"];

const DEFERRED_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CLIPBOARD_TRANSFERS: usize = 32;
const MAX_SOURCE_TRANSFERS: usize = 8;

pub(crate) struct Clipboard {
    connection: Connection,
    loop_handle: LoopHandle<'static, WaylandClientStatePtr>,
    self_mime_nonce: Uuid,

    sources: Rc<RefCell<ClipboardSources>>,
    primary_contents: Option<ClipboardItem>,

    selection: ExternalSelection<WlDataOffer>,
    primary_selection: ExternalSelection<ZwpPrimarySelectionOfferV1>,
}

pub(crate) trait ReceiveData {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>);
}

impl ReceiveData for WlDataOffer {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

impl ReceiveData for ZwpPrimarySelectionOfferV1 {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

#[derive(Clone, Debug)]
/// Wrapper for `WlDataOffer` and `ZwpPrimarySelectionOfferV1`, used to help track mime types.
pub(crate) struct DataOffer<T: ReceiveData> {
    pub inner: T,
    mime_types: Vec<String>,
}

impl<T: ReceiveData> DataOffer<T> {
    pub fn new(offer: T) -> Self {
        Self {
            inner: offer,
            mime_types: Vec::new(),
        }
    }

    pub fn add_mime_type(&mut self, mime_type: String) {
        self.mime_types.push(mime_type)
    }

    fn has_mime_type(&self, mime_type: &str) -> bool {
        self.mime_types
            .iter()
            .any(|candidate| candidate == mime_type)
    }

    fn read_bytes(&self, connection: &Connection, mime_type: &str) -> Option<Vec<u8>> {
        let pipe = match Pipe::new() {
            Ok(pipe) => pipe,
            Err(error) => {
                log::error!("error creating clipboard pipe: {error:?}");
                return None;
            }
        };
        self.inner.receive_data(mime_type.to_string(), unsafe {
            BorrowedFd::borrow_raw(pipe.write.as_raw_fd())
        });
        let fd = pipe.read;
        drop(pipe.write);

        if let Err(error) = connection.flush() {
            log::error!("error flushing clipboard receive request: {error:?}");
            return None;
        }

        match read_fd_with_timeout(fd, PIPE_READ_TIMEOUT) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                log::error!("error reading clipboard pipe: {error:?}");
                None
            }
        }
    }

    fn read_text(&self, connection: &Connection) -> Option<ClipboardItem> {
        let mime_type = self.mime_types.iter().find(|mime_type| {
            ALLOWED_TEXT_MIME_TYPES
                .iter()
                .any(|allowed| allowed == mime_type)
        })?;
        let bytes = self.read_bytes(connection, mime_type)?;
        let text_content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(error) => {
                log::error!("Failed to convert clipboard content to UTF-8: {error}");
                return None;
            }
        };

        let result = text_content.replace("\r\n", "\n");
        Some(ClipboardItem::new_string(result))
    }

    fn read_image(&self, connection: &Connection) -> Option<ClipboardItem> {
        for format in ImageFormat::iter() {
            let mime_type = format.mime_type();
            if !self.has_mime_type(mime_type) {
                continue;
            }

            if let Some(bytes) = self.read_bytes(connection, mime_type) {
                let id = hash(&bytes);
                return Some(ClipboardItem {
                    entries: vec![ClipboardEntry::Image(Image { format, bytes, id })],
                });
            }
        }
        None
    }
}

struct ExternalSelection<T: ReceiveData> {
    cached_read: Option<ClipboardItem>,
    current_offer: Option<DataOffer<T>>,
}

impl<T: ReceiveData> ExternalSelection<T> {
    fn new() -> Self {
        Self {
            cached_read: None,
            current_offer: None,
        }
    }

    fn set_offer(&mut self, offer: Option<DataOffer<T>>) {
        self.cached_read = None;
        self.current_offer = offer;
    }
}

struct ClipboardSources {
    sources: HashMap<Uuid, ClipboardSource>,
    transfers: HashMap<Uuid, ClipboardTransfer>,
}

struct ClipboardSource {
    data: ClipboardSourceData,
    transfers: Vec<Uuid>,
    retire_when_idle: bool,
}

enum ClipboardSourceData {
    Ready(ClipboardItem),
    PendingImage,
    Failed,
}

struct ClipboardTransfer {
    source_id: Uuid,
    state: ClipboardTransferState,
    timeout_token: Option<RegistrationToken>,
    write_token: Option<RegistrationToken>,
}

enum ClipboardTransferState {
    Pending(OwnedFd),
    Sending,
}

enum SendData {
    Ready(Vec<u8>),
    Pending,
    Unavailable,
}

impl ClipboardSources {
    fn new() -> Self {
        Self {
            sources: HashMap::default(),
            transfers: HashMap::default(),
        }
    }

    fn insert_ready(&mut self, source_id: Uuid, item: ClipboardItem, retire_when_idle: bool) {
        self.sources.insert(
            source_id,
            ClipboardSource {
                data: ClipboardSourceData::Ready(item),
                transfers: Vec::new(),
                retire_when_idle,
            },
        );
    }

    fn insert_pending(&mut self, source_id: Uuid) {
        self.sources.insert(
            source_id,
            ClipboardSource {
                data: ClipboardSourceData::PendingImage,
                transfers: Vec::new(),
                retire_when_idle: false,
            },
        );
    }

    fn item(&self, source_id: Uuid) -> Option<ClipboardItem> {
        match &self.sources.get(&source_id)?.data {
            ClipboardSourceData::Ready(item) => Some(item.clone()),
            ClipboardSourceData::PendingImage | ClipboardSourceData::Failed => None,
        }
    }

    fn send_data(&self, source_id: Uuid, mime_type: &str) -> SendData {
        let Some(source) = self.sources.get(&source_id) else {
            return SendData::Unavailable;
        };
        match &source.data {
            ClipboardSourceData::PendingImage if mime_type == ImageFormat::Png.mime_type() => {
                SendData::Pending
            }
            ClipboardSourceData::PendingImage | ClipboardSourceData::Failed => {
                SendData::Unavailable
            }
            ClipboardSourceData::Ready(item) => {
                if TEXT_MIME_TYPES.contains(&mime_type) {
                    return item
                        .text()
                        .map(|text| SendData::Ready(text.into_bytes()))
                        .unwrap_or(SendData::Unavailable);
                }

                let Some(format) = ImageFormat::from_mime_type(mime_type) else {
                    return SendData::Unavailable;
                };
                item.entries()
                    .iter()
                    .find_map(|entry| match entry {
                        ClipboardEntry::Image(image) if image.format == format => {
                            Some(SendData::Ready(image.bytes.clone()))
                        }
                        _ => None,
                    })
                    .unwrap_or(SendData::Unavailable)
            }
        }
    }

    fn offered_mime_types(&self, source_id: Uuid) -> Vec<&'static str> {
        let Some(source) = self.sources.get(&source_id) else {
            return Vec::new();
        };
        match &source.data {
            ClipboardSourceData::PendingImage => vec![ImageFormat::Png.mime_type()],
            ClipboardSourceData::Failed => Vec::new(),
            ClipboardSourceData::Ready(item) => {
                let mut mime_types = Vec::new();
                if item.text().is_some() {
                    mime_types.extend(TEXT_MIME_TYPES);
                }
                for entry in item.entries() {
                    if let ClipboardEntry::Image(image) = entry
                        && !mime_types.contains(&image.format.mime_type())
                    {
                        mime_types.push(image.format.mime_type());
                    }
                }
                mime_types
            }
        }
    }

    fn enqueue_transfer(&mut self, source_id: Uuid, fd: OwnedFd) -> Option<Uuid> {
        if self.transfers.len() >= MAX_CLIPBOARD_TRANSFERS {
            return None;
        }
        let source = self.sources.get_mut(&source_id)?;
        if source.transfers.len() >= MAX_SOURCE_TRANSFERS {
            return None;
        }

        let transfer_id = Uuid::new_v4();
        source.transfers.push(transfer_id);
        self.transfers.insert(
            transfer_id,
            ClipboardTransfer {
                source_id,
                state: ClipboardTransferState::Pending(fd),
                timeout_token: None,
                write_token: None,
            },
        );
        Some(transfer_id)
    }

    fn set_timeout_token(&mut self, transfer_id: Uuid, token: RegistrationToken) -> bool {
        let Some(transfer) = self.transfers.get_mut(&transfer_id) else {
            return false;
        };
        transfer.timeout_token = Some(token);
        true
    }

    fn begin_sending(&mut self, transfer_id: Uuid) -> Option<OwnedFd> {
        let transfer = self.transfers.get_mut(&transfer_id)?;
        let ClipboardTransferState::Pending(fd) =
            std::mem::replace(&mut transfer.state, ClipboardTransferState::Sending)
        else {
            return None;
        };
        Some(fd)
    }

    fn set_write_token(&mut self, transfer_id: Uuid, token: RegistrationToken) -> bool {
        let Some(transfer) = self.transfers.get_mut(&transfer_id) else {
            return false;
        };
        transfer.write_token = Some(token);
        true
    }

    fn fulfill(
        &mut self,
        source_id: Uuid,
        png_bytes: Vec<u8>,
    ) -> Result<Vec<Uuid>, DeferredClipboardImageError> {
        let Some(source) = self.sources.get_mut(&source_id) else {
            return Err(DeferredClipboardImageError::Retired);
        };
        if !matches!(source.data, ClipboardSourceData::PendingImage) {
            return Err(DeferredClipboardImageError::Retired);
        }

        let id = hash(&png_bytes);
        source.data = ClipboardSourceData::Ready(ClipboardItem::new_image(&Image {
            format: ImageFormat::Png,
            bytes: png_bytes,
            id,
        }));
        Ok(source.transfers.clone())
    }

    fn fail(
        &mut self,
        source_id: Uuid,
    ) -> Result<Vec<ClipboardTransfer>, DeferredClipboardImageError> {
        let Some(source) = self.sources.get_mut(&source_id) else {
            return Err(DeferredClipboardImageError::Retired);
        };
        if !matches!(source.data, ClipboardSourceData::PendingImage) {
            return Err(DeferredClipboardImageError::Retired);
        }
        source.data = ClipboardSourceData::Failed;
        let transfer_ids = std::mem::take(&mut source.transfers);
        Ok(transfer_ids
            .into_iter()
            .filter_map(|transfer_id| self.transfers.remove(&transfer_id))
            .collect())
    }

    fn cancel(&mut self, source_id: Uuid) -> Vec<ClipboardTransfer> {
        let Some(source) = self.sources.remove(&source_id) else {
            return Vec::new();
        };
        source
            .transfers
            .into_iter()
            .filter_map(|transfer_id| self.transfers.remove(&transfer_id))
            .collect()
    }

    fn remove_transfer(&mut self, transfer_id: Uuid) -> Option<ClipboardTransfer> {
        let transfer = self.transfers.remove(&transfer_id)?;
        let retire_source = if let Some(source) = self.sources.get_mut(&transfer.source_id) {
            source
                .transfers
                .retain(|candidate| *candidate != transfer_id);
            source.retire_when_idle && source.transfers.is_empty()
        } else {
            false
        };
        if retire_source {
            self.sources.remove(&transfer.source_id);
        }
        Some(transfer)
    }

    fn finish_from_write(&mut self, transfer_id: Uuid) -> Option<RegistrationToken> {
        self.remove_transfer(transfer_id)?.timeout_token
    }

    fn finish_from_timeout(&mut self, transfer_id: Uuid) -> Option<RegistrationToken> {
        self.remove_transfer(transfer_id)?.write_token
    }
}

impl Clipboard {
    pub fn new(
        connection: Connection,
        loop_handle: LoopHandle<'static, WaylandClientStatePtr>,
    ) -> Self {
        Self {
            connection,
            loop_handle,
            self_mime_nonce: Uuid::new_v4(),
            sources: Rc::new(RefCell::new(ClipboardSources::new())),
            primary_contents: None,
            selection: ExternalSelection::new(),
            primary_selection: ExternalSelection::new(),
        }
    }

    pub fn set(&self, source_id: Uuid, item: ClipboardItem) {
        self.sources
            .borrow_mut()
            .insert_ready(source_id, item, false);
    }

    pub fn begin_deferred(&self, source_id: Uuid) {
        self.sources.borrow_mut().insert_pending(source_id);
    }

    pub fn fulfill_deferred(
        &self,
        source_id: Uuid,
        png_bytes: Vec<u8>,
    ) -> Result<(), DeferredClipboardImageError> {
        fulfill_source(&self.loop_handle, &self.sources, source_id, png_bytes)
    }

    pub fn fail_deferred(&self, source_id: Uuid) -> Result<(), DeferredClipboardImageError> {
        fail_source(&self.loop_handle, &self.sources, source_id)
    }

    pub fn cancel_source(&self, source_id: Uuid) {
        cancel_source(&self.loop_handle, &self.sources, source_id);
    }

    pub fn set_primary(&mut self, item: ClipboardItem) {
        self.primary_contents = Some(item);
    }

    pub fn set_offer(&mut self, data_offer: Option<DataOffer<WlDataOffer>>) {
        self.selection.set_offer(data_offer);
    }

    pub fn set_primary_offer(&mut self, data_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>) {
        self.primary_selection.set_offer(data_offer);
    }

    pub fn self_mime(&self, source_id: Uuid) -> String {
        format!(
            "application/x-zed-clipboard-{}-{source_id}",
            self.self_mime_nonce
        )
    }

    pub fn primary_self_mime(&self) -> String {
        format!("application/x-zed-primary-{}", self.self_mime_nonce)
    }

    pub fn offered_mime_types(&self, source_id: Uuid) -> Vec<&'static str> {
        self.sources.borrow().offered_mime_types(source_id)
    }

    pub fn send(&self, source_id: Uuid, mime_type: String, fd: OwnedFd) {
        queue_source_send(
            &self.loop_handle,
            &self.sources,
            source_id,
            &mime_type,
            fd,
            DEFERRED_SEND_TIMEOUT,
        );
    }

    pub fn send_primary(&self, _mime_type: String, fd: OwnedFd) {
        if let Some(text) = self
            .primary_contents
            .as_ref()
            .and_then(|contents| contents.text())
        {
            self.send_bytes(fd, text.into_bytes());
        }
    }

    pub fn read(&mut self) -> Option<ClipboardItem> {
        let offer = self.selection.current_offer.as_ref()?;
        if let Some(cached) = self.selection.cached_read.clone() {
            return Some(cached);
        }

        let sources = self.sources.borrow();
        let own_source_id = sources
            .sources
            .keys()
            .find(|source_id| offer.has_mime_type(&self.self_mime(**source_id)))
            .copied();
        if let Some(source_id) = own_source_id {
            return sources.item(source_id);
        }
        drop(sources);

        let item = offer
            .read_text(&self.connection)
            .or_else(|| offer.read_image(&self.connection))?;
        self.selection.cached_read = Some(item.clone());
        Some(item)
    }

    pub fn read_primary(&mut self) -> Option<ClipboardItem> {
        let offer = self.primary_selection.current_offer.as_ref()?;
        if let Some(cached) = self.primary_selection.cached_read.clone() {
            return Some(cached);
        }

        if offer.has_mime_type(&self.primary_self_mime()) {
            return self.primary_contents.clone();
        }

        let item = offer
            .read_text(&self.connection)
            .or_else(|| offer.read_image(&self.connection))?;
        self.primary_selection.cached_read = Some(item.clone());
        Some(item)
    }

    pub fn send_bytes(&self, fd: OwnedFd, bytes: Vec<u8>) {
        if let Err(error) = set_nonblocking(&fd) {
            log::error!("failed to make clipboard transfer nonblocking: {error:?}");
            return;
        }
        let source_id = Uuid::new_v4();
        self.sources.borrow_mut().insert_ready(
            source_id,
            ClipboardItem::new_string(String::new()),
            true,
        );
        let Some(transfer_id) = self.sources.borrow_mut().enqueue_transfer(source_id, fd) else {
            self.sources.borrow_mut().cancel(source_id);
            return;
        };
        if !insert_transfer_timeout(
            &self.loop_handle,
            &self.sources,
            transfer_id,
            DEFERRED_SEND_TIMEOUT,
        ) {
            self.sources.borrow_mut().cancel(source_id);
            return;
        }
        start_sending(&self.loop_handle, &self.sources, transfer_id, bytes);
    }
}

fn queue_source_send<Data: 'static>(
    loop_handle: &LoopHandle<'static, Data>,
    sources: &Rc<RefCell<ClipboardSources>>,
    source_id: Uuid,
    mime_type: &str,
    fd: OwnedFd,
    timeout: Duration,
) {
    if let Err(error) = set_nonblocking(&fd) {
        log::error!("failed to make clipboard transfer nonblocking: {error:?}");
        return;
    }

    let send_data = sources.borrow().send_data(source_id, mime_type);
    if matches!(send_data, SendData::Unavailable) {
        return;
    }
    let Some(transfer_id) = sources.borrow_mut().enqueue_transfer(source_id, fd) else {
        log::warn!("dropping clipboard transfer because the bounded queue is full");
        return;
    };
    if !insert_transfer_timeout(loop_handle, sources, transfer_id, timeout) {
        sources.borrow_mut().remove_transfer(transfer_id);
        return;
    }
    if let SendData::Ready(bytes) = send_data {
        start_sending(loop_handle, sources, transfer_id, bytes);
    }
}

fn fulfill_source<Data: 'static>(
    loop_handle: &LoopHandle<'static, Data>,
    sources: &Rc<RefCell<ClipboardSources>>,
    source_id: Uuid,
    png_bytes: Vec<u8>,
) -> Result<(), DeferredClipboardImageError> {
    let image_reader =
        image::ImageReader::with_format(Cursor::new(&png_bytes), image::ImageFormat::Png);
    if image_reader.into_dimensions().is_err() {
        fail_source(loop_handle, sources, source_id)?;
        return Err(DeferredClipboardImageError::InvalidPng);
    }

    let transfer_ids = sources.borrow_mut().fulfill(source_id, png_bytes)?;
    for transfer_id in transfer_ids {
        let send_data = sources
            .borrow()
            .send_data(source_id, ImageFormat::Png.mime_type());
        if let SendData::Ready(bytes) = send_data {
            start_sending(loop_handle, sources, transfer_id, bytes);
        }
    }
    Ok(())
}

fn fail_source<Data>(
    loop_handle: &LoopHandle<'static, Data>,
    sources: &Rc<RefCell<ClipboardSources>>,
    source_id: Uuid,
) -> Result<(), DeferredClipboardImageError> {
    let transfers = sources.borrow_mut().fail(source_id)?;
    remove_transfers(loop_handle, transfers);
    Ok(())
}

fn cancel_source<Data>(
    loop_handle: &LoopHandle<'static, Data>,
    sources: &Rc<RefCell<ClipboardSources>>,
    source_id: Uuid,
) {
    let transfers = sources.borrow_mut().cancel(source_id);
    remove_transfers(loop_handle, transfers);
}

fn insert_transfer_timeout<Data: 'static>(
    loop_handle: &LoopHandle<'static, Data>,
    sources: &Rc<RefCell<ClipboardSources>>,
    transfer_id: Uuid,
    timeout: Duration,
) -> bool {
    let weak_sources = Rc::downgrade(sources);
    let weak_loop_handle = loop_handle.downgrade();
    let timer = Timer::from_duration(timeout);
    let token = match loop_handle.insert_source(timer, move |_, _, _| {
        if let Some(sources) = weak_sources.upgrade() {
            let write_token = sources.borrow_mut().finish_from_timeout(transfer_id);
            if let (Some(loop_handle), Some(write_token)) =
                (weak_loop_handle.upgrade(), write_token)
            {
                loop_handle.remove(write_token);
            }
        }
        TimeoutAction::Drop
    }) {
        Ok(token) => token,
        Err(error) => {
            log::error!("failed to insert clipboard transfer timeout: {error:?}");
            return false;
        }
    };

    if sources.borrow_mut().set_timeout_token(transfer_id, token) {
        true
    } else {
        loop_handle.remove(token);
        false
    }
}

fn start_sending<Data: 'static>(
    loop_handle: &LoopHandle<'static, Data>,
    sources: &Rc<RefCell<ClipboardSources>>,
    transfer_id: Uuid,
    bytes: Vec<u8>,
) {
    let Some(fd) = sources.borrow_mut().begin_sending(transfer_id) else {
        return;
    };
    let weak_sources: Weak<RefCell<ClipboardSources>> = Rc::downgrade(sources);
    let weak_loop_handle = loop_handle.downgrade();
    let mut written = 0;
    let token = match loop_handle.insert_source(
        calloop::generic::Generic::new(
            File::from(fd),
            calloop::Interest::WRITE,
            calloop::Mode::Level,
        ),
        move |_, file, _| {
            let file = unsafe { file.get_mut() };
            let (action, finished) = loop {
                match file.write(&bytes[written..]) {
                    Ok(0) => break (PostAction::Remove, true),
                    Ok(count) if written + count == bytes.len() => {
                        break (PostAction::Remove, true);
                    }
                    Ok(count) => written += count,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        break (PostAction::Continue, false);
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => {
                        log::debug!("clipboard transfer ended with an error: {error:?}");
                        break (PostAction::Remove, true);
                    }
                }
            };
            if finished && let Some(sources) = weak_sources.upgrade() {
                let timeout_token = sources.borrow_mut().finish_from_write(transfer_id);
                if let (Some(loop_handle), Some(timeout_token)) =
                    (weak_loop_handle.upgrade(), timeout_token)
                {
                    loop_handle.remove(timeout_token);
                }
            }
            Ok(action)
        },
    ) {
        Ok(token) => token,
        Err(error) => {
            log::error!("failed to insert clipboard transfer: {error:?}");
            if let Some(transfer) = sources.borrow_mut().remove_transfer(transfer_id)
                && let Some(timeout_token) = transfer.timeout_token
            {
                loop_handle.remove(timeout_token);
            }
            return;
        }
    };

    if !sources.borrow_mut().set_write_token(transfer_id, token) {
        loop_handle.remove(token);
    }
}

fn remove_transfers<Data>(
    loop_handle: &LoopHandle<'static, Data>,
    transfers: Vec<ClipboardTransfer>,
) {
    for transfer in transfers {
        if let Some(token) = transfer.timeout_token {
            loop_handle.remove(token);
        }
        if let Some(token) = transfer.write_token {
            loop_handle.remove(token);
        }
    }
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{ErrorKind, Read as _},
        os::fd::{FromRawFd as _, IntoRawFd as _},
    };

    use calloop::EventLoop;

    use super::*;

    fn png_bytes(marker: u8) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([marker, 0, 0, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("PNG encoding failed");
        bytes.into_inner()
    }

    fn pipe_pair() -> (filedescriptor::FileDescriptor, OwnedFd) {
        let Pipe { read, write } = Pipe::new().expect("pipe creation failed");
        let write = unsafe { OwnedFd::from_raw_fd(write.into_raw_fd()) };
        (read, write)
    }

    fn read_all(mut read: filedescriptor::FileDescriptor) -> Vec<u8> {
        let mut bytes = Vec::new();
        read.read_to_end(&mut bytes).expect("pipe read failed");
        bytes
    }

    fn make_source_store() -> Rc<RefCell<ClipboardSources>> {
        Rc::new(RefCell::new(ClipboardSources::new()))
    }

    #[test]
    fn ordinary_png_source_sends_externally_consumable_bytes() {
        let mut event_loop = EventLoop::<()>::try_new().expect("event loop creation failed");
        let sources = make_source_store();
        let source_id = Uuid::new_v4();
        let bytes = png_bytes(1);
        sources.borrow_mut().insert_ready(
            source_id,
            ClipboardItem::new_image(&Image {
                format: ImageFormat::Png,
                bytes: bytes.clone(),
                id: hash(&bytes),
            }),
            false,
        );
        assert_eq!(
            sources.borrow().offered_mime_types(source_id),
            vec!["image/png"]
        );

        let (read, write_fd) = pipe_pair();
        queue_source_send(
            &event_loop.handle(),
            &sources,
            source_id,
            "image/png",
            write_fd,
            Duration::from_secs(1),
        );
        event_loop
            .dispatch(Duration::ZERO, &mut ())
            .expect("event loop dispatch failed");

        assert_eq!(read_all(read), bytes);
        assert!(sources.borrow().item(source_id).is_some());
        cancel_source(&event_loop.handle(), &sources, source_id);
        assert!(sources.borrow().item(source_id).is_none());
    }

    #[test]
    fn pending_send_waits_for_fulfillment_of_its_source() {
        let mut event_loop = EventLoop::<()>::try_new().expect("event loop creation failed");
        let sources = make_source_store();
        let source_id = Uuid::new_v4();
        sources.borrow_mut().insert_pending(source_id);
        let bytes = png_bytes(2);

        let (mut read, write_fd) = pipe_pair();
        read.set_non_blocking(true)
            .expect("failed to make read pipe nonblocking");
        queue_source_send(
            &event_loop.handle(),
            &sources,
            source_id,
            "image/png",
            write_fd,
            Duration::from_secs(1),
        );
        let mut buffer = [0; 1];
        let error = read.read(&mut buffer).expect_err("pending pipe was ready");
        assert_eq!(error.kind(), ErrorKind::WouldBlock);

        fulfill_source(&event_loop.handle(), &sources, source_id, bytes.clone())
            .expect("source fulfillment failed");
        event_loop
            .dispatch(Duration::ZERO, &mut ())
            .expect("event loop dispatch failed");
        assert_eq!(read_all(read), bytes);
    }

    #[test]
    fn pending_send_times_out_and_closes_its_fd() {
        let mut event_loop = EventLoop::<()>::try_new().expect("event loop creation failed");
        let sources = make_source_store();
        let source_id = Uuid::new_v4();
        sources.borrow_mut().insert_pending(source_id);
        let (read, write_fd) = pipe_pair();
        queue_source_send(
            &event_loop.handle(),
            &sources,
            source_id,
            "image/png",
            write_fd,
            Duration::from_millis(5),
        );
        event_loop
            .dispatch(Duration::from_millis(50), &mut ())
            .expect("event loop dispatch failed");

        assert!(read_all(read).is_empty());
        assert!(sources.borrow().transfers.is_empty());
    }

    #[test]
    fn failure_and_cancellation_close_pending_fds_and_retire_work() {
        let event_loop = EventLoop::<()>::try_new().expect("event loop creation failed");
        let sources = make_source_store();

        let failed_source = Uuid::new_v4();
        sources.borrow_mut().insert_pending(failed_source);
        let (failed_read, failed_write_fd) = pipe_pair();
        queue_source_send(
            &event_loop.handle(),
            &sources,
            failed_source,
            "image/png",
            failed_write_fd,
            Duration::from_secs(1),
        );
        fail_source(&event_loop.handle(), &sources, failed_source).expect("source failure failed");
        assert!(read_all(failed_read).is_empty());
        assert_eq!(
            fulfill_source(&event_loop.handle(), &sources, failed_source, png_bytes(3)),
            Err(DeferredClipboardImageError::Retired)
        );

        let cancelled_source = Uuid::new_v4();
        sources.borrow_mut().insert_pending(cancelled_source);
        let (cancelled_read, cancelled_write_fd) = pipe_pair();
        queue_source_send(
            &event_loop.handle(),
            &sources,
            cancelled_source,
            "image/png",
            cancelled_write_fd,
            Duration::from_secs(1),
        );
        cancel_source(&event_loop.handle(), &sources, cancelled_source);
        assert!(read_all(cancelled_read).is_empty());
        assert_eq!(
            fulfill_source(
                &event_loop.handle(),
                &sources,
                cancelled_source,
                png_bytes(4)
            ),
            Err(DeferredClipboardImageError::Retired)
        );
    }

    #[test]
    fn invalid_png_fails_the_source_and_closes_pending_fds() {
        let event_loop = EventLoop::<()>::try_new().expect("event loop creation failed");
        let sources = make_source_store();
        let source_id = Uuid::new_v4();
        sources.borrow_mut().insert_pending(source_id);
        let (read, write_fd) = pipe_pair();
        queue_source_send(
            &event_loop.handle(),
            &sources,
            source_id,
            "image/png",
            write_fd,
            Duration::from_secs(1),
        );

        let result = fulfill_source(
            &event_loop.handle(),
            &sources,
            source_id,
            b"\x89PNG\r\n\x1a\ninvalid".to_vec(),
        );

        assert_eq!(result, Err(DeferredClipboardImageError::InvalidPng));
        assert!(read_all(read).is_empty());
        assert!(sources.borrow().item(source_id).is_none());
        assert!(sources.borrow().transfers.is_empty());
    }

    #[test]
    fn cancellation_closes_an_active_bounded_send() {
        let mut event_loop = EventLoop::<()>::try_new().expect("event loop creation failed");
        let sources = make_source_store();
        let source_id = Uuid::new_v4();
        let bytes = vec![7; 1024 * 1024];
        sources.borrow_mut().insert_ready(
            source_id,
            ClipboardItem::new_image(&Image {
                format: ImageFormat::Png,
                bytes: bytes.clone(),
                id: hash(&bytes),
            }),
            false,
        );
        let (read, write_fd) = pipe_pair();
        queue_source_send(
            &event_loop.handle(),
            &sources,
            source_id,
            "image/png",
            write_fd,
            Duration::from_secs(1),
        );
        event_loop
            .dispatch(Duration::ZERO, &mut ())
            .expect("event loop dispatch failed");
        assert_eq!(sources.borrow().transfers.len(), 1);

        cancel_source(&event_loop.handle(), &sources, source_id);

        let sent = read_all(read);
        assert!(sent.len() < bytes.len());
        assert!(sources.borrow().transfers.is_empty());
        assert!(sources.borrow().item(source_id).is_none());
    }

    #[test]
    fn reversed_fulfillment_keeps_each_source_data_independent() {
        let mut event_loop = EventLoop::<()>::try_new().expect("event loop creation failed");
        let sources = make_source_store();
        let source_a = Uuid::new_v4();
        let source_b = Uuid::new_v4();
        sources.borrow_mut().insert_pending(source_a);
        sources.borrow_mut().insert_pending(source_b);

        let (read_a, write_a) = pipe_pair();
        queue_source_send(
            &event_loop.handle(),
            &sources,
            source_a,
            "image/png",
            write_a,
            Duration::from_secs(1),
        );
        let (read_b, write_b) = pipe_pair();
        queue_source_send(
            &event_loop.handle(),
            &sources,
            source_b,
            "image/png",
            write_b,
            Duration::from_secs(1),
        );

        let bytes_a = png_bytes(10);
        let bytes_b = png_bytes(20);
        fulfill_source(&event_loop.handle(), &sources, source_b, bytes_b.clone())
            .expect("source B fulfillment failed");
        fulfill_source(&event_loop.handle(), &sources, source_a, bytes_a.clone())
            .expect("source A fulfillment failed");
        event_loop
            .dispatch(Duration::ZERO, &mut ())
            .expect("event loop dispatch failed");

        assert_eq!(read_all(read_a), bytes_a);
        assert_eq!(read_all(read_b), bytes_b);
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeOffer(u8);

    impl ReceiveData for FakeOffer {
        fn receive_data(&self, _mime_type: String, _fd: BorrowedFd<'_>) {}
    }

    fn fake_offer(id: u8) -> DataOffer<FakeOffer> {
        let mut offer = DataOffer::new(FakeOffer(id));
        offer.add_mime_type("image/png".to_string());
        offer
    }

    #[test]
    fn equal_and_aba_external_offers_always_replace_the_previous_offer() {
        let mut selection = ExternalSelection::new();
        selection.cached_read = Some(ClipboardItem::new_string("cached".to_string()));
        selection.set_offer(Some(fake_offer(1)));
        assert!(selection.cached_read.is_none());
        assert_eq!(
            selection.current_offer.as_ref().map(|offer| &offer.inner),
            Some(&FakeOffer(1))
        );

        selection.cached_read = Some(ClipboardItem::new_string("same payload".to_string()));
        selection.set_offer(Some(fake_offer(2)));
        assert!(selection.cached_read.is_none());
        assert_eq!(
            selection.current_offer.as_ref().map(|offer| &offer.inner),
            Some(&FakeOffer(2))
        );

        selection.set_offer(Some(fake_offer(1)));
        assert_eq!(
            selection.current_offer.as_ref().map(|offer| &offer.inner),
            Some(&FakeOffer(1))
        );
    }

    #[test]
    fn late_old_fulfillment_does_not_mutate_a_newer_external_offer() {
        let event_loop = EventLoop::<()>::try_new().expect("event loop creation failed");
        let sources = make_source_store();
        let old_source = Uuid::new_v4();
        sources.borrow_mut().insert_pending(old_source);
        let mut selection = ExternalSelection::new();
        selection.set_offer(Some(fake_offer(9)));

        fulfill_source(&event_loop.handle(), &sources, old_source, png_bytes(30))
            .expect("old source fulfillment failed");

        assert_eq!(
            selection.current_offer.as_ref().map(|offer| &offer.inner),
            Some(&FakeOffer(9))
        );
        assert!(sources.borrow().item(old_source).is_some());
    }

    #[test]
    fn pending_transfer_queue_is_bounded_per_source() {
        let event_loop = EventLoop::<()>::try_new().expect("event loop creation failed");
        let sources = make_source_store();
        let source_id = Uuid::new_v4();
        sources.borrow_mut().insert_pending(source_id);
        let mut readers = Vec::new();

        for _ in 0..MAX_SOURCE_TRANSFERS {
            let (read, write_fd) = pipe_pair();
            queue_source_send(
                &event_loop.handle(),
                &sources,
                source_id,
                "image/png",
                write_fd,
                Duration::from_secs(1),
            );
            readers.push(read);
        }
        let (rejected_read, rejected_write_fd) = pipe_pair();
        queue_source_send(
            &event_loop.handle(),
            &sources,
            source_id,
            "image/png",
            rejected_write_fd,
            Duration::from_secs(1),
        );

        assert!(read_all(rejected_read).is_empty());
        assert_eq!(sources.borrow().transfers.len(), MAX_SOURCE_TRANSFERS);
        cancel_source(&event_loop.handle(), &sources, source_id);
        for reader in readers {
            assert!(read_all(reader).is_empty());
        }
    }
}
