// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::fs::{File, OpenOptions};
use std::io::{Error as IoError, Read, Result as IoResult, Write};
use std::os::{
    raw::*,
    unix::{
        fs::{FileTypeExt, OpenOptionsExt},
        io::{AsRawFd, FromRawFd, RawFd},
    },
};
use std::path::Path;

use net_gen::ifreq;
use utils::{
    ioctl::{ioctl_with_mut_ref, ioctl_with_ref, ioctl_with_val},
    ioctl_expr, ioctl_ioc_nr, ioctl_iow_nr,
    net::macvtap::MacVTap,
};

// As defined in the Linux UAPI:
// https://elixir.bootlin.com/linux/v4.17/source/include/uapi/linux/if.h#L33
const IFACE_NAME_MAX_LEN: usize = 16;

/// List of errors the tap implementation can throw.
#[derive(Debug)]
pub enum Error {
    /// Unable to create tap interface.
    CreateTap(IoError),
    /// Invalid interface name.
    InvalidIfname,
    /// Tap interface device is not a character device.
    InvalidTapDevType,
    /// ioctl failed.
    IoctlError(IoError),
    /// Unable to open tap interface device.
    OpenTapDev(IoError),
    /// Couldn't open /dev/net/tun.
    OpenTun(IoError),
    /// Unable to stat tap interface device for macvtap interface.
    StatTapDev(IoError),
}

pub type Result<T> = ::std::result::Result<T, Error>;

const TUNTAP: ::std::os::raw::c_uint = 84;
ioctl_iow_nr!(TUNSETIFF, TUNTAP, 202, ::std::os::raw::c_int);
ioctl_iow_nr!(TUNSETOFFLOAD, TUNTAP, 208, ::std::os::raw::c_uint);
ioctl_iow_nr!(TUNSETVNETHDRSZ, TUNTAP, 216, ::std::os::raw::c_int);

/// Handle for a network tap interface.
///
/// For now, this simply wraps the file descriptor for the tap device so methods
/// can run ioctls on the interface. The tap interface fd will be closed when
/// Tap goes out of scope, and the kernel will clean up the interface automatically.
#[derive(Debug)]
pub struct Tap {
    tap_file: File,
    pub(crate) if_name: [u8; IFACE_NAME_MAX_LEN],
}

// Returns a byte vector representing the contents of a null terminated C string which
// contains if_name.
fn build_terminated_if_name(if_name: &str) -> Result<[u8; IFACE_NAME_MAX_LEN]> {
    // Convert the string slice to bytes, and shadow the variable,
    // since we no longer need the &str version.
    let if_name = if_name.as_bytes();

    if if_name.len() >= IFACE_NAME_MAX_LEN {
        return Err(Error::InvalidIfname);
    }

    let mut terminated_if_name = [b'\0'; IFACE_NAME_MAX_LEN];
    terminated_if_name[..if_name.len()].copy_from_slice(if_name);

    Ok(terminated_if_name)
}

pub struct IfReqBuilder(ifreq);

impl IfReqBuilder {
    pub fn new() -> Self {
        Self(Default::default())
    }

    pub fn if_name(mut self, if_name: &[u8; IFACE_NAME_MAX_LEN]) -> Self {
        // SAFETY: Since we don't call as_mut on the same union field more than once, this block is
        // safe.
        let ifrn_name = unsafe { self.0.ifr_ifrn.ifrn_name.as_mut() };
        ifrn_name.copy_from_slice(if_name.as_ref());

        self
    }

    pub(crate) fn flags(mut self, flags: i16) -> Self {
        self.0.ifr_ifru.ifru_flags = flags;
        self
    }

    pub(crate) fn execute<F: AsRawFd>(mut self, socket: &F, ioctl: u64) -> Result<ifreq> {
        // SAFETY: ioctl is safe. Called with a valid socket fd, and we check the return.
        let ret = unsafe { ioctl_with_mut_ref(socket, ioctl, &mut self.0) };
        if ret < 0 {
            return Err(Error::IoctlError(IoError::last_os_error()));
        }

        Ok(self.0)
    }
}

impl Tap {
    /// * `if_name` - the name of the interface.
    /// Create a TUN/TAP device given the tap or macvtap interface name.
    /// # Arguments
    ///
    /// * `if_name` - the name of the interface.
    pub fn open_named(if_name: &str) -> Result<Tap> {
        // Options:
        //  - /dev/net/<if_name> exists; open it.
        //  - It's a macvtap device: determine by checking /sys; open the
        //    corresponding /dev/tapX node.
        //  - It's a tap device: open /dev/net/tun and allocate via SETIFF.
        if let Ok(path) = MacVTap::get_device_node(if_name) {
            Self::macvtap_open_named(if_name, &path)
        } else {
            Self::tap_open_named(if_name)
        }
    }

    /// Create a TUN/TAP device given the macvtap interface name and device node.
    /// # Arguments
    ///
    /// * `if_name` - the name of the interface.
    /// * `dev_path` - location of the interface's device node.
    fn macvtap_open_named(if_name: &str, dev_path: &Path) -> Result<Tap> {
        // Open the device node
        let mut opts = OpenOptions::new();
        let tap_file = opts
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(dev_path)
            .map_err(Error::OpenTapDev)?;

        // Must be a char device
        let md = tap_file.metadata().map_err(Error::StatTapDev)?;
        if !md.file_type().is_char_device() {
            return Err(Error::InvalidTapDevType);
        }

        // The length check is probably unnecessary because we know that the
        // network interface is valid at this point, but it doesn't hurt.
        let name_bytes = if_name.as_bytes();
        if name_bytes.len() >= IFACE_NAME_MAX_LEN {
            return Err(Error::InvalidIfname);
        }

        let mut ret = Tap {
            tap_file,
            if_name: [0; IFACE_NAME_MAX_LEN],
        };

        ret.if_name[..name_bytes.len()].copy_from_slice(name_bytes);
        Ok(ret)
    }

    /// Create a TUN/TAP device given the tap interface name.
    /// # Arguments
    ///
    /// * `if_name` - the name of the interface.
    fn tap_open_named(if_name: &str) -> Result<Tap> {
        let terminated_if_name = build_terminated_if_name(if_name)?;

        // SAFETY: Open calls are safe because we give a constant null-terminated
        // string and verify the result.
        let fd = unsafe {
            libc::open(
                b"/dev/net/tun\0".as_ptr().cast::<c_char>(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(Error::OpenTun(IoError::last_os_error()));
        }
        // SAFETY: We just checked that the fd is valid.
        let tuntap = unsafe { File::from_raw_fd(fd) };

        let ifreq = IfReqBuilder::new()
            .if_name(&terminated_if_name)
            .flags((net_gen::IFF_TAP | net_gen::IFF_NO_PI | net_gen::IFF_VNET_HDR) as i16)
            .execute(&tuntap, TUNSETIFF())?;

        Ok(Tap {
            tap_file: tuntap,
            // SAFETY: Safe since only the name is accessed, and it's cloned out.
            if_name: unsafe { ifreq.ifr_ifrn.ifrn_name },
        })
    }

    pub fn if_name_as_str(&self) -> &str {
        let len = self
            .if_name
            .iter()
            .position(|x| *x == 0)
            .unwrap_or(IFACE_NAME_MAX_LEN);
        std::str::from_utf8(&self.if_name[..len]).unwrap_or("")
    }

    /// Set the offload flags for the tap interface.
    pub fn set_offload(&self, flags: c_uint) -> Result<()> {
        // SAFETY: ioctl is safe. Called with a valid tap fd, and we check the return.
        let ret = unsafe { ioctl_with_val(&self.tap_file, TUNSETOFFLOAD(), c_ulong::from(flags)) };
        if ret < 0 {
            return Err(Error::IoctlError(IoError::last_os_error()));
        }

        Ok(())
    }

    /// Set the size of the vnet hdr.
    pub fn set_vnet_hdr_size(&self, size: c_int) -> Result<()> {
        // SAFETY: ioctl is safe. Called with a valid tap fd, and we check the return.
        let ret = unsafe { ioctl_with_ref(&self.tap_file, TUNSETVNETHDRSZ(), &size) };
        if ret < 0 {
            return Err(Error::IoctlError(IoError::last_os_error()));
        }

        Ok(())
    }
}

impl Read for Tap {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        self.tap_file.read(buf)
    }
}

impl Write for Tap {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        self.tap_file.write(buf)
    }

    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

impl AsRawFd for Tap {
    fn as_raw_fd(&self) -> RawFd {
        self.tap_file.as_raw_fd()
    }
}

#[cfg(test)]
pub mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]

    use std::os::unix::ffi::OsStrExt;

    use net_gen::ETH_HLEN;

    use super::*;
    use crate::virtio::net::test_utils::{enable, if_index, TapTrafficSimulator};

    // The size of the virtio net header
    const VNET_HDR_SIZE: usize = 10;

    const PAYLOAD_SIZE: usize = 512;
    const PACKET_SIZE: usize = 1024;

    #[test]
    fn test_tap_name() {
        // Sanity check that the assumed max iface name length is correct.
        assert_eq!(IFACE_NAME_MAX_LEN, unsafe {
            net_gen::ifreq__bindgen_ty_1::default().ifrn_name.len()
        });

        // Empty name - The tap should be named "tap0" by default
        let tap = Tap::open_named("").unwrap();
        assert_eq!(b"tap0\0\0\0\0\0\0\0\0\0\0\0\0", &tap.if_name);
        assert_eq!("tap0", tap.if_name_as_str());

        // 16 characters - too long.
        let name = "a123456789abcdef";
        match Tap::open_named(name) {
            Err(Error::InvalidIfname) => (),
            _ => panic!("Expected Error::InvalidIfname"),
        };

        // 15 characters - OK.
        let name = "a123456789abcde";
        let tap = Tap::open_named(name).unwrap();
        assert_eq!(&format!("{}\0", name).as_bytes(), &tap.if_name);
        assert_eq!(name, tap.if_name_as_str());
    }

    #[test]
    fn test_tap_exclusive_open() {
        let _tap1 = Tap::open_named("exclusivetap").unwrap();
        // Opening same tap device a second time should not be permitted.
        Tap::open_named("exclusivetap").unwrap_err();
    }

    #[test]
    fn test_set_options() {
        // This line will fail to provide an initialized FD if the test is not run as root.
        let tap = Tap::open_named("").unwrap();
        tap.set_vnet_hdr_size(16).unwrap();
        tap.set_offload(0).unwrap();

        let faulty_tap = Tap {
            tap_file: unsafe { File::from_raw_fd(-2) },
            if_name: [0x01; 16],
        };
        assert!(faulty_tap.set_vnet_hdr_size(16).is_err());
        assert!(faulty_tap.set_offload(0).is_err());
    }

    #[test]
    fn test_raw_fd() {
        let tap = Tap::open_named("").unwrap();
        assert_eq!(tap.as_raw_fd(), tap.tap_file.as_raw_fd());
    }

    #[test]
    fn test_read() {
        let mut tap = Tap::open_named("").unwrap();
        enable(&tap);
        let tap_traffic_simulator = TapTrafficSimulator::new(if_index(&tap));

        let packet = utils::rand::rand_alphanumerics(PAYLOAD_SIZE);
        tap_traffic_simulator.push_tx_packet(packet.as_bytes());

        let mut buf = [0u8; PACKET_SIZE];
        assert!(tap.read(&mut buf).is_ok());
        assert_eq!(
            &buf[VNET_HDR_SIZE..packet.len() + VNET_HDR_SIZE],
            packet.as_bytes()
        );
    }

    #[test]
    fn test_write() {
        let mut tap = Tap::open_named("").unwrap();
        enable(&tap);
        let tap_traffic_simulator = TapTrafficSimulator::new(if_index(&tap));

        let mut packet = [0u8; PACKET_SIZE];
        let payload = utils::rand::rand_alphanumerics(PAYLOAD_SIZE);
        packet[ETH_HLEN as usize..payload.len() + ETH_HLEN as usize]
            .copy_from_slice(payload.as_bytes());
        assert!(tap.write(&packet).is_ok());

        let mut read_buf = [0u8; PACKET_SIZE];
        assert!(tap_traffic_simulator.pop_rx_packet(&mut read_buf));
        assert_eq!(
            &read_buf[..PACKET_SIZE - VNET_HDR_SIZE],
            &packet[VNET_HDR_SIZE..]
        );
    }
}
