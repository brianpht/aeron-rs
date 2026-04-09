// CnC (Command and Control) file - memory-mapped shared state between
// the media driver and client processes.
//
// Layout:
//   [CncHeader]                    - 256 bytes (version, PID, timestamps)
//   [to_driver ring buffer]        - to_driver_capacity + 128 (trailer)
//   [to_clients broadcast buffer]  - to_clients_capacity + 128 (trailer)
//
// All sizes are power-of-two. The file is created by the driver and
// mapped by clients. Uses mmap (libc) for zero-copy IPC.
//
// No allocation in steady state. All ring buffers are pre-sized.

use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};

use super::ring_buffer::{self, MpscRingBuffer};
use super::broadcast::{self, BroadcastTransmitter, BroadcastReceiver};

// ---- CnC Header ----

/// Version of the CnC file layout. Clients check this before mapping.
pub const CNC_VERSION: i32 = 1;

/// CnC header size (padded to 256 bytes for alignment).
const CNC_HEADER_LENGTH: usize = 256;

/// Offsets within the CnC header.
const CNC_VERSION_OFFSET: usize = 0;
const PID_OFFSET: usize = 4;
const TO_DRIVER_CAPACITY_OFFSET: usize = 8;
const TO_CLIENTS_CAPACITY_OFFSET: usize = 12;
const DRIVER_HEARTBEAT_OFFSET: usize = 16;
// Bytes 24..256: reserved (zeroed).

// ---- Computed offsets ----

/// Offset of the to-driver ring buffer data region within the CnC file.
const fn to_driver_offset() -> usize {
    CNC_HEADER_LENGTH
}

/// Offset of the to-clients broadcast buffer data region within the CnC file.
const fn to_clients_offset(to_driver_capacity: usize) -> usize {
    to_driver_offset() + ring_buffer::required_buffer_size(to_driver_capacity)
}

/// Total CnC file size.
pub fn cnc_file_length(to_driver_capacity: usize, to_clients_capacity: usize) -> usize {
    CNC_HEADER_LENGTH
        + ring_buffer::required_buffer_size(to_driver_capacity)
        + broadcast::required_buffer_size(to_clients_capacity)
}

// ---- Error types ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CncError {
    /// Capacity must be a power-of-two >= 32.
    InvalidCapacity,
    /// mmap or file creation failed.
    IoError(i32),
    /// CnC version mismatch.
    VersionMismatch,
    /// CnC file header not yet ready (driver still initializing).
    NotReady,
}

impl std::fmt::Display for CncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCapacity => f.write_str("capacity must be power-of-two >= 32"),
            Self::IoError(errno) => write!(f, "io error: errno {errno}"),
            Self::VersionMismatch => f.write_str("CnC version mismatch"),
            Self::NotReady => f.write_str("CnC file not ready"),
        }
    }
}

// ---- CncFile: driver-side (create + write) ----

/// Driver-side CnC file handle. Creates the mmap'd file and provides
/// access to the to-driver ring buffer (for reading commands) and
/// the to-clients broadcast buffer (for writing responses).
pub struct DriverCnc {
    /// Pointer to the mmap'd region.
    base: *mut u8,
    /// Total mapped length.
    length: usize,
    /// To-driver ring buffer (MPSC, conductor reads commands).
    to_driver: MpscRingBuffer,
    /// To-clients broadcast buffer (conductor writes responses).
    to_clients: BroadcastTransmitter,
    /// Data region capacities (for offset computation).
    to_driver_capacity: usize,
    to_clients_capacity: usize,
}

impl std::fmt::Debug for DriverCnc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverCnc")
            .field("length", &self.length)
            .field("to_driver_capacity", &self.to_driver_capacity)
            .field("to_clients_capacity", &self.to_clients_capacity)
            .finish()
    }
}

// SAFETY: DriverCnc is used only by the conductor agent (single-threaded).
// The mmap'd memory is shared with clients via the file, but the conductor
// only accesses the ring buffer consumer side and the broadcast producer side.
unsafe impl Send for DriverCnc {}

impl DriverCnc {
    /// Create a new CnC file backed by anonymous mmap (no filesystem path).
    ///
    /// For testing and single-process use. Multi-process use requires a
    /// file-backed mmap (see `create_file`).
    pub fn create_anonymous(
        to_driver_capacity: usize,
        to_clients_capacity: usize,
    ) -> Result<Self, CncError> {
        Self::validate_capacities(to_driver_capacity, to_clients_capacity)?;

        let length = cnc_file_length(to_driver_capacity, to_clients_capacity);

        // Allocate via anonymous mmap.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_SHARED,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(CncError::IoError(unsafe { *libc::__errno_location() }));
        }
        let base = base as *mut u8;

        let result = unsafe {
            Self::init(base, length, to_driver_capacity, to_clients_capacity)
        };

        match result {
            Ok(cnc) => Ok(cnc),
            Err(e) => {
                unsafe { libc::munmap(base as *mut _, length); }
                Err(e)
            }
        }
    }

    /// Create a CnC file at the given filesystem path.
    ///
    /// The file is created, sized via ftruncate, then mmap'd with MAP_SHARED.
    /// Client processes can then map the same file for IPC.
    pub fn create_file(
        path: &str,
        to_driver_capacity: usize,
        to_clients_capacity: usize,
    ) -> Result<Self, CncError> {
        Self::validate_capacities(to_driver_capacity, to_clients_capacity)?;

        let length = cnc_file_length(to_driver_capacity, to_clients_capacity);

        // Create/truncate the file.
        let c_path = std::ffi::CString::new(path)
            .map_err(|_| CncError::IoError(libc::EINVAL))?;

        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                0o660,
            )
        };
        if fd < 0 {
            return Err(CncError::IoError(unsafe { *libc::__errno_location() }));
        }

        // Size the file.
        if unsafe { libc::ftruncate(fd, length as libc::off_t) } != 0 {
            let errno = unsafe { *libc::__errno_location() };
            unsafe { libc::close(fd); }
            return Err(CncError::IoError(errno));
        }

        // mmap the file.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        unsafe { libc::close(fd); }

        if base == libc::MAP_FAILED {
            return Err(CncError::IoError(unsafe { *libc::__errno_location() }));
        }
        let base = base as *mut u8;

        let result = unsafe {
            Self::init(base, length, to_driver_capacity, to_clients_capacity)
        };

        match result {
            Ok(cnc) => Ok(cnc),
            Err(e) => {
                unsafe { libc::munmap(base as *mut _, length); }
                Err(e)
            }
        }
    }

    fn validate_capacities(
        to_driver_capacity: usize,
        to_clients_capacity: usize,
    ) -> Result<(), CncError> {
        if to_driver_capacity < 32 || !to_driver_capacity.is_power_of_two() {
            return Err(CncError::InvalidCapacity);
        }
        if to_clients_capacity < 32 || !to_clients_capacity.is_power_of_two() {
            return Err(CncError::InvalidCapacity);
        }
        Ok(())
    }

    /// Initialize the CnC header and create ring buffer handles.
    ///
    /// # Safety
    ///
    /// `base` must point to at least `length` zero-initialized bytes.
    unsafe fn init(
        base: *mut u8,
        length: usize,
        to_driver_capacity: usize,
        to_clients_capacity: usize,
    ) -> Result<Self, CncError> {
        // Zero the entire region (mmap provides zeroed pages, but be safe).
        unsafe { std::ptr::write_bytes(base, 0, length); }

        // Create ring buffer handles.
        let to_driver_ptr = unsafe { base.add(to_driver_offset()) };
        let to_driver = unsafe { MpscRingBuffer::new(to_driver_ptr, to_driver_capacity) }
            .map_err(|_| CncError::InvalidCapacity)?;

        let to_clients_ptr = unsafe { base.add(to_clients_offset(to_driver_capacity)) };
        let to_clients = unsafe { BroadcastTransmitter::new(to_clients_ptr, to_clients_capacity) }
            .map_err(|_| CncError::InvalidCapacity)?;

        // Write the CnC header last (version field is the "ready" signal).
        let hdr = base;
        unsafe {
            write_i32_raw(hdr, PID_OFFSET, std::process::id() as i32);
            write_i32_raw(hdr, TO_DRIVER_CAPACITY_OFFSET, to_driver_capacity as i32);
            write_i32_raw(hdr, TO_CLIENTS_CAPACITY_OFFSET, to_clients_capacity as i32);

            // Write driver heartbeat timestamp.
            let heartbeat = hdr.add(DRIVER_HEARTBEAT_OFFSET) as *const AtomicI64;
            (*heartbeat).store(current_epoch_ms(), Ordering::Release);

            // Write version last (signals to clients that the file is ready).
            // Use Release so all prior writes are visible.
            let version_ptr = hdr.add(CNC_VERSION_OFFSET) as *const AtomicI32;
            (*version_ptr).store(CNC_VERSION, Ordering::Release);
        }

        Ok(Self {
            base,
            length,
            to_driver,
            to_clients,
            to_driver_capacity,
            to_clients_capacity,
        })
    }

    /// Read commands from the to-driver ring buffer.
    pub fn to_driver(&self) -> &MpscRingBuffer {
        &self.to_driver
    }

    /// Write responses to the to-clients broadcast buffer.
    pub fn to_clients(&self) -> &BroadcastTransmitter {
        &self.to_clients
    }

    /// Update the driver heartbeat timestamp.
    pub fn update_heartbeat(&self) {
        unsafe {
            let heartbeat = self.base.add(DRIVER_HEARTBEAT_OFFSET) as *const AtomicI64;
            (*heartbeat).store(current_epoch_ms(), Ordering::Release);
        }
    }

    /// Get the raw base pointer (for creating client-side handles in tests).
    pub fn base_ptr(&self) -> *mut u8 {
        self.base
    }

    /// Total mapped length.
    pub fn length(&self) -> usize {
        self.length
    }

    /// To-driver capacity.
    pub fn to_driver_capacity(&self) -> usize {
        self.to_driver_capacity
    }

    /// To-clients capacity.
    pub fn to_clients_capacity(&self) -> usize {
        self.to_clients_capacity
    }
}

impl Drop for DriverCnc {
    fn drop(&mut self) {
        if !self.base.is_null() {
            // Clear version to signal clients that the driver is shutting down.
            unsafe {
                let version_ptr = self.base.add(CNC_VERSION_OFFSET) as *const AtomicI32;
                (*version_ptr).store(0, Ordering::Release);
                libc::munmap(self.base as *mut _, self.length);
            }
            self.base = std::ptr::null_mut();
        }
    }
}

// ---- ClientCnc: client-side (map + read/write) ----

/// Client-side CnC handle. Maps an existing CnC file and provides
/// access to the to-driver ring buffer (for writing commands) and
/// the to-clients broadcast buffer (for reading responses).
pub struct ClientCnc {
    base: *mut u8,
    length: usize,
    to_driver: MpscRingBuffer,
    to_clients_rx: BroadcastReceiver,
    /// Whether this handle owns the mmap and should munmap on drop.
    /// `false` when created via `from_ptr` (shared address space).
    /// `true` when created via `map_file` (client owns its own mapping).
    owns_memory: bool,
}

impl std::fmt::Debug for ClientCnc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCnc")
            .field("length", &self.length)
            .finish()
    }
}

unsafe impl Send for ClientCnc {}

impl ClientCnc {
    /// Map an existing CnC file from the given base pointer.
    ///
    /// Used in-process (e.g. tests) when the driver and client share
    /// the same address space.
    ///
    /// # Safety
    ///
    /// The base pointer must point to a valid, initialized CnC region.
    pub unsafe fn from_ptr(base: *mut u8, length: usize) -> Result<Self, CncError> {
        // Read and validate version.
        let version = unsafe {
            let version_ptr = base.add(CNC_VERSION_OFFSET) as *const AtomicI32;
            (*version_ptr).load(Ordering::Acquire)
        };
        if version == 0 {
            return Err(CncError::NotReady);
        }
        if version != CNC_VERSION {
            return Err(CncError::VersionMismatch);
        }

        let to_driver_capacity = unsafe { read_i32_raw(base, TO_DRIVER_CAPACITY_OFFSET) } as usize;
        let to_clients_capacity = unsafe { read_i32_raw(base, TO_CLIENTS_CAPACITY_OFFSET) } as usize;

        let to_driver_ptr = unsafe { base.add(to_driver_offset()) };
        let to_driver = unsafe { MpscRingBuffer::new(to_driver_ptr, to_driver_capacity) }
            .map_err(|_| CncError::InvalidCapacity)?;

        let to_clients_ptr = unsafe { base.add(to_clients_offset(to_driver_capacity)) };
        let to_clients_tx_tmp = unsafe { BroadcastTransmitter::new(to_clients_ptr, to_clients_capacity) }
            .map_err(|_| CncError::InvalidCapacity)?;
        let cursor = to_clients_tx_tmp.tail_counter();
        let to_clients_rx = unsafe {
            BroadcastReceiver::new(
                to_clients_ptr as *const u8,
                to_clients_capacity,
                cursor,
            )
        };

        Ok(Self {
            base,
            length,
            to_driver,
            to_clients_rx,
            owns_memory: false,
        })
    }

    /// Map an existing CnC file from the given filesystem path.
    pub fn map_file(path: &str) -> Result<Self, CncError> {
        let c_path = std::ffi::CString::new(path)
            .map_err(|_| CncError::IoError(libc::EINVAL))?;

        let fd = unsafe {
            libc::open(c_path.as_ptr(), libc::O_RDWR)
        };
        if fd < 0 {
            return Err(CncError::IoError(unsafe { *libc::__errno_location() }));
        }

        // Get file size.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            let errno = unsafe { *libc::__errno_location() };
            unsafe { libc::close(fd); }
            return Err(CncError::IoError(errno));
        }
        let length = stat.st_size as usize;

        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        unsafe { libc::close(fd); }

        if base == libc::MAP_FAILED {
            return Err(CncError::IoError(unsafe { *libc::__errno_location() }));
        }

        let mut client = unsafe { Self::from_ptr(base as *mut u8, length)? };
        client.owns_memory = true;
        Ok(client)
    }

    /// Write a command to the to-driver ring buffer.
    pub fn to_driver(&self) -> &MpscRingBuffer {
        &self.to_driver
    }

    /// Read responses from the to-clients broadcast buffer.
    pub fn to_clients(&mut self) -> &mut BroadcastReceiver {
        &mut self.to_clients_rx
    }

    /// Check if the driver is alive by reading its heartbeat timestamp.
    pub fn is_driver_alive(&self, timeout_ms: i64) -> bool {
        let now = current_epoch_ms();
        unsafe {
            let heartbeat = self.base.add(DRIVER_HEARTBEAT_OFFSET) as *const AtomicI64;
            let last = (*heartbeat).load(Ordering::Acquire);
            now.wrapping_sub(last) < timeout_ms
        }
    }

    /// Driver PID from the CnC header.
    pub fn driver_pid(&self) -> i32 {
        unsafe { read_i32_raw(self.base, PID_OFFSET) }
    }
}

impl Drop for ClientCnc {
    fn drop(&mut self) {
        if self.owns_memory && !self.base.is_null() {
            unsafe { libc::munmap(self.base as *mut _, self.length); }
            self.base = std::ptr::null_mut();
        }
    }
}

// ---- Helpers ----

fn current_epoch_ms() -> i64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts); }
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

unsafe fn write_i32_raw(base: *mut u8, offset: usize, val: i32) {
    let bytes = val.to_le_bytes();
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(offset), 4); }
}

unsafe fn read_i32_raw(base: *const u8, offset: usize) -> i32 {
    let mut bytes = [0u8; 4];
    unsafe { std::ptr::copy_nonoverlapping(base.add(offset), bytes.as_mut_ptr(), 4); }
    i32::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TO_DRIVER: usize = 1024;
    const TEST_TO_CLIENTS: usize = 1024;

    #[test]
    fn create_anonymous_cnc() {
        let cnc = DriverCnc::create_anonymous(TEST_TO_DRIVER, TEST_TO_CLIENTS)
            .expect("create");
        assert_eq!(cnc.to_driver_capacity(), TEST_TO_DRIVER);
        assert_eq!(cnc.to_clients_capacity(), TEST_TO_CLIENTS);
    }

    #[test]
    fn invalid_capacity() {
        assert_eq!(
            DriverCnc::create_anonymous(100, TEST_TO_CLIENTS).unwrap_err(),
            CncError::InvalidCapacity,
        );
        assert_eq!(
            DriverCnc::create_anonymous(TEST_TO_DRIVER, 100).unwrap_err(),
            CncError::InvalidCapacity,
        );
    }

    #[test]
    fn client_maps_driver_cnc() {
        let cnc = DriverCnc::create_anonymous(TEST_TO_DRIVER, TEST_TO_CLIENTS)
            .expect("create");

        let client = unsafe {
            ClientCnc::from_ptr(cnc.base_ptr(), cnc.length())
        }.expect("client map");

        assert_eq!(client.driver_pid(), std::process::id() as i32);
    }

    #[test]
    fn driver_heartbeat_alive() {
        let cnc = DriverCnc::create_anonymous(TEST_TO_DRIVER, TEST_TO_CLIENTS)
            .expect("create");
        cnc.update_heartbeat();

        let client = unsafe {
            ClientCnc::from_ptr(cnc.base_ptr(), cnc.length())
        }.expect("client map");

        assert!(client.is_driver_alive(5000));
    }

    #[test]
    fn command_roundtrip_via_cnc() {
        let cnc = DriverCnc::create_anonymous(TEST_TO_DRIVER, TEST_TO_CLIENTS)
            .expect("create");

        let mut client = unsafe {
            ClientCnc::from_ptr(cnc.base_ptr(), cnc.length())
        }.expect("client map");

        // Client writes a command.
        use crate::cnc::command::*;
        let cmd = AddPublication::from_channel(1, 1, 10, "aeron:udp?endpoint=127.0.0.1:40123")
            .expect("cmd");
        let mut buf = [0u8; ADD_PUBLICATION_LENGTH];
        cmd.encode(&mut buf).expect("encode");
        client.to_driver().write(CMD_ADD_PUBLICATION, &buf).expect("write");

        // Driver reads the command.
        let mut received_type = 0i32;
        let mut received_data = Vec::new();
        cnc.to_driver().read(|msg_type, data| {
            received_type = msg_type;
            received_data = data.to_vec();
        });
        assert_eq!(received_type, CMD_ADD_PUBLICATION);
        let decoded = AddPublication::decode(&received_data).expect("decode");
        assert_eq!(decoded.stream_id, 10);

        // Driver writes a response.
        let rsp = PublicationReady {
            correlation_id: 1,
            registration_id: 100,
            session_id: 42,
            stream_id: 10,
            position_limit_counter_id: 0,
            channel_status_indicator_id: 1,
        };
        let mut rsp_buf = [0u8; PUBLICATION_READY_LENGTH];
        rsp.encode(&mut rsp_buf).expect("encode rsp");
        cnc.to_clients().transmit(RSP_PUBLICATION_READY, &rsp_buf).expect("transmit");

        // Client reads the response.
        let mut rsp_type = 0i32;
        let mut rsp_data = Vec::new();
        client.to_clients().receive(|msg_type, data| {
            rsp_type = msg_type;
            rsp_data = data.to_vec();
        });
        assert_eq!(rsp_type, RSP_PUBLICATION_READY);
        let decoded_rsp = PublicationReady::decode(&rsp_data).expect("decode rsp");
        assert_eq!(decoded_rsp.session_id, 42);
        assert_eq!(decoded_rsp.registration_id, 100);
    }

    #[test]
    fn cnc_file_length_correct() {
        let expected = 256
            + ring_buffer::required_buffer_size(1024)
            + broadcast::required_buffer_size(1024);
        assert_eq!(cnc_file_length(1024, 1024), expected);
    }

    #[test]
    fn client_not_ready_before_version() {
        // Create raw anonymous mmap (zeroed, no version written).
        let length = cnc_file_length(TEST_TO_DRIVER, TEST_TO_CLIENTS);
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_SHARED,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);

        let result = unsafe {
            ClientCnc::from_ptr(base as *mut u8, length)
        };
        assert_eq!(result.unwrap_err(), CncError::NotReady);

        unsafe { libc::munmap(base, length); }
    }

    #[test]
    fn display_errors() {
        assert!(CncError::InvalidCapacity.to_string().contains("power-of-two"));
        assert!(CncError::VersionMismatch.to_string().contains("mismatch"));
        assert!(CncError::NotReady.to_string().contains("not ready"));
    }
}

