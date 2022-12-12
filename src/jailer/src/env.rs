// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::{CStr, OsString};
use std::fs::{self, canonicalize, File, OpenOptions, Permissions};
use std::io;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::IntoRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{cgroup, to_cstring};
use crate::{Error, Result};
use utils::arg_parser::Error::MissingValue;
use utils::net::macvtap::MacVTap;
use utils::syscall::SyscallReturnCode;
use utils::{arg_parser, validators};

use crate::cgroup::{Cgroup, CgroupBuilder};
use crate::chroot::chroot;
use crate::resource_limits::{ResourceLimits, FSIZE_ARG, NO_FILE_ARG};

const STDIN_FILENO: libc::c_int = 0;
const STDOUT_FILENO: libc::c_int = 1;
const STDERR_FILENO: libc::c_int = 2;

// Kernel-based virtual machine (hardware virtualization extensions)
// minor/major numbers are taken from
// https://www.kernel.org/doc/html/latest/admin-guide/devices.html
const DEV_KVM_WITH_NUL: &[u8] = b"/dev/kvm\0";
const DEV_KVM_MAJOR: u32 = 10;
const DEV_KVM_MINOR: u32 = 232;

// TUN/TAP device minor/major numbers are taken from
// www.kernel.org/doc/Documentation/networking/tuntap.txt
const DEV_NET_TUN_WITH_NUL: &[u8] = b"/dev/net/tun\0";
const DEV_NET_TUN_MAJOR: u32 = 10;
const DEV_NET_TUN_MINOR: u32 = 200;

// Random number generator device minor/major numbers are taken from
// https://www.kernel.org/doc/Documentation/admin-guide/devices.txt
const DEV_URANDOM_WITH_NUL: &[u8] = b"/dev/urandom\0";
const DEV_URANDOM_MAJOR: u32 = 1;
const DEV_URANDOM_MINOR: u32 = 9;

// Relevant folders inside the jail that we create or/and for which we change ownership.
// We need /dev in order to be able to create /dev/kvm and /dev/net/tun device.
// We need /run for the default location of the api socket.
// Since libc::chown is not recursive, we cannot specify only /dev/net as we want
// to walk through the entire folder hierarchy.
const FOLDER_HIERARCHY: [&[u8]; 4] = [b"/\0", b"/dev\0", b"/dev/net\0", b"/run\0"];
const FOLDER_PERMISSIONS: u32 = 0o700;

// When running with `--new-pid-ns` flag, the PID of the process running the exec_file differs
// from jailer's and it is stored inside a dedicated file, prefixed with the below extension.
const PID_FILE_EXTENSION: &str = ".pid";

// Helper function, since we'll use libc::dup2 a bunch of times for daemonization.
fn dup2(old_fd: libc::c_int, new_fd: libc::c_int) -> Result<()> {
    // SAFETY: This is safe because we are using a library function with valid parameters.
    SyscallReturnCode(unsafe { libc::dup2(old_fd, new_fd) })
        .into_empty_result()
        .map_err(Error::Dup2)
}

// This is a wrapper for the clone system call. When we want to create a new process in a new
// pid namespace, we will call clone with a NULL stack pointer. We can do this because we will
// not use the CLONE_VM flag, this will result with the original stack replicated, in a similar
// manner to the fork syscall. The libc wrapper prevents use of a NULL stack pointer, so we will
// call the syscall directly.
fn clone(child_stack: *mut libc::c_void, flags: libc::c_int) -> Result<libc::c_int> {
    // Clone parameters order is different between x86_64 and aarch64.
    #[cfg(target_arch = "x86_64")]
    // SAFETY: This is safe because we are using a library function with valid parameters.
    return SyscallReturnCode(unsafe {
        libc::syscall(libc::SYS_clone, flags, child_stack, 0, 0, 0) as libc::c_int
    })
    .into_result()
    .map_err(Error::Clone);
    #[cfg(target_arch = "aarch64")]
    // SAFETY: This is safe because we are using a library function with valid parameters.
    return SyscallReturnCode(unsafe {
        libc::syscall(libc::SYS_clone, flags, child_stack, 0, 0, 0) as libc::c_int
    })
    .into_result()
    .map_err(Error::Clone);
}

pub struct Env {
    id: String,
    chroot_dir: PathBuf,
    exec_file_path: PathBuf,
    uid: u32,
    gid: u32,
    netns: Option<String>,
    daemonize: bool,
    new_pid_ns: bool,
    start_time_us: u64,
    start_time_cpu_us: u64,
    jailer_cpu_time_us: u64,
    extra_args: Vec<String>,
    cgroups: Vec<Box<dyn Cgroup>>,
    resource_limits: ResourceLimits,
    macvtaps: Vec<String>,
}

impl Env {
    pub fn new(
        arguments: &arg_parser::Arguments,
        start_time_us: u64,
        start_time_cpu_us: u64,
    ) -> Result<Self> {
        // Unwraps should not fail because the arguments are mandatory arguments or with default
        // values.
        let id = arguments
            .single_value("id")
            .ok_or_else(|| Error::ArgumentParsing(MissingValue("id".to_string())))?;

        validators::validate_instance_id(id).map_err(Error::InvalidInstanceId)?;

        let exec_file = arguments
            .single_value("exec-file")
            .ok_or_else(|| Error::ArgumentParsing(MissingValue("exec-file".to_string())))?;
        let (exec_file_path, exec_file_name) = Env::validate_exec_file(exec_file)?;

        let chroot_base = arguments
            .single_value("chroot-base-dir")
            .ok_or_else(|| Error::ArgumentParsing(MissingValue("chroot-base-dir".to_string())))?;
        let mut chroot_dir = canonicalize(&chroot_base)
            .map_err(|err| Error::Canonicalize(PathBuf::from(&chroot_base), err))?;

        if !chroot_dir.is_dir() {
            return Err(Error::NotADirectory(chroot_dir));
        }

        chroot_dir.push(&exec_file_name);
        chroot_dir.push(id);
        chroot_dir.push("root");

        let uid_str = arguments
            .single_value("uid")
            .ok_or_else(|| Error::ArgumentParsing(MissingValue("uid".to_string())))?;
        let uid = uid_str
            .parse::<u32>()
            .map_err(|_| Error::Uid(uid_str.to_owned()))?;

        let gid_str = arguments
            .single_value("gid")
            .ok_or_else(|| Error::ArgumentParsing(MissingValue("gid".to_string())))?;
        let gid = gid_str
            .parse::<u32>()
            .map_err(|_| Error::Gid(gid_str.to_owned()))?;

        let netns = arguments.single_value("netns").cloned();

        let daemonize = arguments.flag_present("daemonize");

        let new_pid_ns = arguments.flag_present("new-pid-ns");

        // Optional arguments.
        let mut cgroups: Vec<Box<dyn Cgroup>> = Vec::new();
        let parent_cgroup = match arguments.single_value("parent-cgroup") {
            Some(parent_cg) => Path::new(parent_cg),
            None => Path::new(&exec_file_name),
        };
        if parent_cgroup
            .components()
            .any(|c| c == Component::CurDir || c == Component::ParentDir || c == Component::RootDir)
        {
            return Err(Error::CgroupInvalidParentPath());
        }

        let cgroup_ver = arguments
            .single_value("cgroup-version")
            .ok_or_else(|| Error::ArgumentParsing(MissingValue("cgroup-version".to_string())))?;
        let cgroup_ver = cgroup_ver
            .parse::<u8>()
            .map_err(|_| Error::CgroupInvalidVersion(cgroup_ver.to_string()))?;

        let mut cgroup_builder = None;

        // cgroup format: <cgroup_controller>.<cgroup_property>=<value>,...
        if let Some(cgroups_args) = arguments.multiple_values("cgroup") {
            let builder = cgroup_builder.get_or_insert(CgroupBuilder::new(cgroup_ver)?);
            for cg in cgroups_args {
                let aux: Vec<&str> = cg.split('=').collect();
                if aux.len() != 2 || aux[1].is_empty() {
                    return Err(Error::CgroupFormat(cg.to_string()));
                }
                let file = Path::new(aux[0]);
                if file.components().any(|c| {
                    c == Component::CurDir || c == Component::ParentDir || c == Component::RootDir
                }) {
                    return Err(Error::CgroupInvalidFile(cg.to_string()));
                }

                let cgroup = builder.new_cgroup(
                    aux[0].to_string(), // cgroup file
                    aux[1].to_string(), // cgroup value
                    id,
                    parent_cgroup,
                )?;
                cgroups.push(cgroup);
            }
        }

        let mut resource_limits = ResourceLimits::default();
        if let Some(args) = arguments.multiple_values("resource-limit") {
            Env::parse_resource_limits(&mut resource_limits, args)?;
        }
        // macvtap arg format: --macvtap if_name => create device node /dev/net/if_name in the chroot.
        let mut macvtaps = Vec::new();
        // We do not create here the MacVTaps since we need to do join_netns before that.
        if let Some(macvtap_args) = arguments.multiple_values("macvtap") {
            for arg in macvtap_args {
                macvtaps.push(arg.to_string());
            }
        }

        Ok(Env {
            id: id.to_owned(),
            chroot_dir,
            exec_file_path,
            uid,
            gid,
            netns,
            daemonize,
            new_pid_ns,
            start_time_us,
            start_time_cpu_us,
            jailer_cpu_time_us: 0,
            extra_args: arguments.extra_args(),
            cgroups,
            resource_limits,
            macvtaps,
        })
    }

    pub fn chroot_dir(&self) -> &Path {
        self.chroot_dir.as_path()
    }

    pub fn gid(&self) -> u32 {
        self.gid
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    fn validate_exec_file(exec_file: &str) -> Result<(PathBuf, String)> {
        let exec_file_path = canonicalize(exec_file)
            .map_err(|err| Error::Canonicalize(PathBuf::from(exec_file), err))?;

        if !exec_file_path.is_file() {
            return Err(Error::NotAFile(exec_file_path));
        }

        let exec_file_name = exec_file_path
            .file_name()
            .ok_or_else(|| Error::ExtractFileName(exec_file_path.clone()))?
            .to_str()
            // Safe to unwrap as the original `exec_file` is `String`.
            .unwrap()
            .to_string();

        if !exec_file_name.contains("firecracker") {
            return Err(Error::ExecFileName(exec_file_name));
        }

        Ok((exec_file_path, exec_file_name))
    }

    fn parse_resource_limits(resource_limits: &mut ResourceLimits, args: &[String]) -> Result<()> {
        for arg in args {
            let (name, value) = arg
                .split_once('=')
                .ok_or_else(|| Error::ResLimitFormat(arg.to_string()))?;

            let limit_value = value
                .parse::<u64>()
                .map_err(|err| Error::ResLimitValue(value.to_string(), err.to_string()))?;
            match name {
                FSIZE_ARG => resource_limits.set_file_size(limit_value),
                NO_FILE_ARG => resource_limits.set_no_file(limit_value),
                _ => return Err(Error::ResLimitArgument(name.to_string())),
            }
        }
        Ok(())
    }

    fn exec_into_new_pid_ns(&mut self, chroot_exec_file: PathBuf) -> Result<()> {
        // Compute jailer's total CPU time up to the current time.
        self.jailer_cpu_time_us =
            utils::time::get_time_us(utils::time::ClockType::ProcessCpu) - self.start_time_cpu_us;

        // Duplicate the current process. The child process will belong to the previously created
        // PID namespace. The current process will not be moved into the newly created namespace,
        // but its first child will assume the role of init(1) in the new namespace.
        let pid = clone(std::ptr::null_mut(), libc::CLONE_NEWPID)?;
        match pid {
            0 => {
                // Reset process start time.
                self.start_time_cpu_us = 0;

                Err(Error::Exec(self.exec_command(chroot_exec_file)))
            }
            child_pid => {
                // Save the PID of the process running the exec file provided
                // inside <chroot_exec_file>.pid file.
                self.save_exec_file_pid(child_pid, chroot_exec_file)?;
                // SAFETY: This is safe because 0 is valid input to exit.
                unsafe { libc::exit(0) }
            }
        }
    }

    fn save_exec_file_pid(&mut self, pid: i32, chroot_exec_file: PathBuf) -> Result<()> {
        let chroot_exec_file_str = chroot_exec_file
            .to_str()
            .ok_or_else(|| Error::ExtractFileName(chroot_exec_file.clone()))?;
        let pid_file_path =
            PathBuf::from(format!("{}{}", chroot_exec_file_str, PID_FILE_EXTENSION));
        let mut pid_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(pid_file_path.clone())
            .map_err(|err| Error::FileOpen(pid_file_path.clone(), err))?;

        // Write PID to file.
        write!(pid_file, "{}", pid).map_err(|err| Error::Write(pid_file_path, err))
    }

    fn mknod_and_own_dev(
        &self,
        dev_path_str: &'static [u8],
        dev_major: u32,
        dev_minor: u32,
    ) -> Result<()> {
        let dev_path = CStr::from_bytes_with_nul(dev_path_str).map_err(Error::FromBytesWithNul)?;
        // As per sysstat.h:
        // S_IFCHR -> character special device
        // S_IRUSR -> read permission, owner
        // S_IWUSR -> write permission, owner
        // See www.kernel.org/doc/Documentation/networking/tuntap.txt, 'Configuration' chapter for
        // more clarity.
        // SAFETY: This is safe because dev_path is CStr, and hence null-terminated.
        SyscallReturnCode(unsafe {
            libc::mknod(
                dev_path.as_ptr(),
                libc::S_IFCHR | libc::S_IRUSR | libc::S_IWUSR,
                libc::makedev(dev_major, dev_minor),
            )
        })
        .into_empty_result()
        .map_err(|err| {
            Error::MknodDev(
                err,
                std::str::from_utf8(dev_path_str).expect("Cannot convert from UTF-8"),
            )
        })?;

        // SAFETY: This is safe because dev_path is CStr, and hence null-terminated.
        SyscallReturnCode(unsafe { libc::chown(dev_path.as_ptr(), self.uid(), self.gid()) })
            .into_empty_result()
            // Safe to unwrap as we provided valid file names.
            .map_err(|err| Error::ChangeFileOwner(PathBuf::from(dev_path.to_str().unwrap()), err))
    }

    fn setup_jailed_folder(&self, folder: &[u8]) -> Result<()> {
        let folder_cstr = CStr::from_bytes_with_nul(folder).map_err(Error::FromBytesWithNul)?;

        // Safe to unwrap as the byte sequence is UTF-8 validated above.
        let path = folder_cstr.to_str().unwrap();
        let path_buf = PathBuf::from(path);
        fs::create_dir_all(path).map_err(|err| Error::CreateDir(path_buf.clone(), err))?;
        fs::set_permissions(path, Permissions::from_mode(FOLDER_PERMISSIONS))
            .map_err(|err| Error::Chmod(path_buf.clone(), err))?;

        #[cfg(target_arch = "x86_64")]
        let folder_bytes_ptr = folder.as_ptr().cast::<i8>();
        #[cfg(target_arch = "aarch64")]
        let folder_bytes_ptr = folder.as_ptr();
        // SAFETY: This is safe because folder was checked for a null-terminator.
        SyscallReturnCode(unsafe { libc::chown(folder_bytes_ptr, self.uid(), self.gid()) })
            .into_empty_result()
            .map_err(|err| Error::ChangeFileOwner(path_buf, err))
    }

    fn copy_exec_to_chroot(&mut self) -> Result<OsString> {
        let exec_file_name = self
            .exec_file_path
            .file_name()
            .ok_or_else(|| Error::ExtractFileName(self.exec_file_path.clone()))?;
        // We do a quick push here to get the global path of the executable inside the chroot,
        // without having to create a new PathBuf. We'll then do a pop to revert to the actual
        // chroot_dir right after the copy.
        // TODO: just now wondering ... is doing a push()/pop() thing better than just creating
        // a new PathBuf, with something like chroot_dir.join(exec_file_name) ?!
        self.chroot_dir.push(exec_file_name);

        // TODO: hard link instead of copy? This would save up disk space, but hard linking is
        // not always possible :(
        fs::copy(&self.exec_file_path, &self.chroot_dir).map_err(|err| {
            Error::Copy(self.exec_file_path.clone(), self.chroot_dir.clone(), err)
        })?;

        // Pop exec_file_name.
        self.chroot_dir.pop();
        Ok(exec_file_name.to_os_string())
    }

    fn join_netns(path: &str) -> Result<()> {
        // The fd backing the file will be automatically dropped at the end of the scope
        let netns_fd = File::open(path)
            .map_err(|e| Error::FileOpen(PathBuf::from(path), e))?
            .into_raw_fd();

        // SAFETY: Safe because we are passing valid parameters.
        SyscallReturnCode(unsafe { libc::setns(netns_fd, libc::CLONE_NEWNET) })
            .into_empty_result()
            .map_err(Error::SetNetNs)?;

        // Since we have ownership here, we also have to close the fd after joining the
        // namespace. Safe because we are passing valid parameters.
        SyscallReturnCode(unsafe { libc::close(netns_fd) })
            .into_empty_result()
            .map_err(Error::CloseNetNsFd)?;

        // Since namespaces are shared by default when creating a new process using fork or clone,
        // unshare() is used to disassociate (unshare) the current process from the mount namespace:
        // https://linux.die.net/man/2/unshare.
        SyscallReturnCode(unsafe { libc::unshare(libc::CLONE_NEWNS) })
            .into_empty_result()
            .map_err(Error::MountSysfs)?;

        // It is not sufficient to join the new network namespace,
        // we also need to mount a version of /sys that describes
        // the network namespace.

        // Don't let any mounts propagate back to the parent.
        // This means that the sysfs of the network namespace will only
        // get mounted for the jailer process.
        SyscallReturnCode(unsafe {
            libc::mount(
                to_cstring("")?.as_ptr(),
                to_cstring("/")?.as_ptr(),
                to_cstring("none")?.as_ptr(),
                libc::MS_SLAVE | libc::MS_REC,
                std::ptr::null(),
            )
        })
        .into_empty_result()
        .map_err(Error::MountSysfs)?;

        // Unmount the current sysfs since it's describing the previous namespace.
        let cstr_sys = to_cstring(Path::new("/sys"))?;
        SyscallReturnCode(unsafe { libc::umount2(cstr_sys.as_ptr(), libc::MNT_DETACH) })
            .into_empty_result()
            .map_err(Error::UmountSysfs)?;

        // Actually mount the sysfs corresponding to the current namespace.
        SyscallReturnCode(unsafe {
            libc::mount(
                std::ptr::null(),
                cstr_sys.as_ptr(),
                to_cstring("sysfs")?.as_ptr(),
                0,
                std::ptr::null(),
            )
        })
        .into_empty_result()
        .map_err(Error::MountSysfs)
    }

    fn exec_command(&self, chroot_exec_file: PathBuf) -> io::Error {
        Command::new(chroot_exec_file)
            .args(&["--id", &self.id])
            .args(&["--start-time-us", &self.start_time_us.to_string()])
            .args(&["--start-time-cpu-us", &self.start_time_cpu_us.to_string()])
            .args(&["--parent-cpu-time-us", &self.jailer_cpu_time_us.to_string()])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .uid(self.uid())
            .gid(self.gid())
            .args(&self.extra_args)
            .exec()
    }

    #[cfg(target_arch = "aarch64")]
    fn copy_cache_info(&self) -> Result<()> {
        use crate::{readln_special, writeln_special};

        const HOST_CACHE_INFO: &str = "/sys/devices/system/cpu/cpu0/cache";
        // Based on https://elixir.free-electrons.com/linux/v4.9.62/source/arch/arm64/kernel/cacheinfo.c#L29.
        const MAX_CACHE_LEVEL: u8 = 7;
        // These are the files that we need to copy in the chroot so that we can create the
        // cache topology.
        const FOLDER_HIERARCHY: [&str; 6] = [
            "size",
            "level",
            "type",
            "shared_cpu_map",
            "coherency_line_size",
            "number_of_sets",
        ];

        // We create the cache folder inside the chroot and then change its permissions.
        let jailer_cache_dir =
            Path::new(self.chroot_dir()).join("sys/devices/system/cpu/cpu0/cache/");
        fs::create_dir_all(&jailer_cache_dir)
            .map_err(|err| Error::CreateDir(jailer_cache_dir.to_owned(), err))?;

        for index in 0..(MAX_CACHE_LEVEL + 1) {
            let index_folder = format!("index{}", index);
            let host_path = PathBuf::from(HOST_CACHE_INFO).join(&index_folder);

            if fs::metadata(&host_path).is_err() {
                // It means the folder does not exist, i.e we exhausted the number of cache levels
                // existent on the host.
                break;
            }

            // We now create the destination folder in the jailer.
            let jailer_path = jailer_cache_dir.join(&index_folder);
            fs::create_dir_all(&jailer_path)
                .map_err(|err| Error::CreateDir(jailer_path.to_owned(), err))?;

            // We now read the contents of the current directory and copy the files we are
            // interested in to the destination path.
            for entry in FOLDER_HIERARCHY.iter() {
                let host_cache_file = host_path.join(&entry);
                let jailer_cache_file = jailer_path.join(&entry);

                let line = readln_special(&host_cache_file)?;
                writeln_special(&jailer_cache_file, line)?;

                // We now change the permissions.
                let dest_path_cstr = to_cstring(&jailer_cache_file)?;
                // SAFETY: Safe because dest_path_cstr is null-terminated.
                SyscallReturnCode(unsafe {
                    libc::chown(dest_path_cstr.as_ptr(), self.uid(), self.gid())
                })
                .into_empty_result()
                .map_err(|err| Error::ChangeFileOwner(jailer_cache_file.to_owned(), err))?;
            }
        }
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    fn copy_midr_el1_info(&self) -> Result<()> {
        use crate::{readln_special, writeln_special};

        const HOST_MIDR_EL1_INFO: &str = "/sys/devices/system/cpu/cpu0/regs/identification";

        let jailer_midr_el1_directory =
            Path::new(self.chroot_dir()).join("sys/devices/system/cpu/cpu0/regs/identification/");
        fs::create_dir_all(&jailer_midr_el1_directory)
            .map_err(|err| Error::CreateDir(jailer_midr_el1_directory.to_owned(), err))?;

        let host_midr_el1_file = PathBuf::from(format!("{}/midr_el1", HOST_MIDR_EL1_INFO));
        let jailer_midr_el1_file = jailer_midr_el1_directory.join("midr_el1");

        // Read and copy the MIDR_EL1 file to Jailer
        let line = readln_special(&host_midr_el1_file)?;
        writeln_special(&jailer_midr_el1_file, line)?;

        // Change the permissions.
        let dest_path_cstr = to_cstring(&jailer_midr_el1_file)?;
        // SAFETY: Safe because `dest_path_cstr` is null-terminated.
        SyscallReturnCode(unsafe { libc::chown(dest_path_cstr.as_ptr(), self.uid(), self.gid()) })
            .into_empty_result()
            .map_err(|err| Error::ChangeFileOwner(jailer_midr_el1_file.to_owned(), err))?;

        Ok(())
    }

    pub fn run(mut self) -> Result<()> {
        let exec_file_name = self.copy_exec_to_chroot()?;
        let chroot_exec_file = PathBuf::from("/").join(&exec_file_name);
        let mut macvtaps = Vec::new();

        // Set limits on resources.
        self.resource_limits.install()?;

        // We have to setup cgroups at this point, because we can't do it anymore after chrooting.
        // cgroups are iterated two times as some cgroups may require others (e.g cpuset requires
        // cpuset.mems and cpuset.cpus) to be set before attaching any pid.
        for cgroup in &self.cgroups {
            // it will panic if any cgroup fails to write
            cgroup.write_value().unwrap();
        }

        for cgroup in &self.cgroups {
            // it will panic if any cgroup fails to attach
            cgroup.attach_pid().unwrap();
        }

        // Join the specified network namespace, if applicable.
        if let Some(ref path) = self.netns {
            Env::join_netns(path)?;
        }

        for macvtap in &self.macvtaps {
            macvtaps.push(
                MacVTap::by_name(&macvtap)
                    .map_err(|e| Error::MacVTapByName(macvtap.to_string(), e))?,
            );
        }

        // If daemonization was requested, open /dev/null before chrooting.
        let dev_null = if self.daemonize {
            Some(File::open("/dev/null").map_err(Error::OpenDevNull)?)
        } else {
            None
        };
        #[cfg(target_arch = "aarch64")]
        self.copy_cache_info()?;
        #[cfg(target_arch = "aarch64")]
        self.copy_midr_el1_info()?;

        // Jail self.
        chroot(self.chroot_dir())?;

        // This will not only create necessary directories, but will also change ownership
        // for all of them.
        FOLDER_HIERARCHY
            .iter()
            .try_for_each(|f| self.setup_jailed_folder(*f))?;

        // Here we are creating the /dev/kvm and /dev/net/tun devices inside the jailer.
        // Following commands can be translated into bash like this:
        // $: mkdir -p $chroot_dir/dev/net
        // $: dev_net_tun_path={$chroot_dir}/"tun"
        // $: mknod $dev_net_tun_path c 10 200
        // www.kernel.org/doc/Documentation/networking/tuntap.txt specifies 10 and 200 as the major
        // and minor for the /dev/net/tun device.
        self.mknod_and_own_dev(DEV_NET_TUN_WITH_NUL, DEV_NET_TUN_MAJOR, DEV_NET_TUN_MINOR)?;
        // Do the same for /dev/kvm with (major, minor) = (10, 232).
        self.mknod_and_own_dev(DEV_KVM_WITH_NUL, DEV_KVM_MAJOR, DEV_KVM_MINOR)?;
        // And for /dev/urandom with (major, minor) = (1, 9).
        // If the device is not accessible on the host, output a warning to inform user that MMDS
        // version 2 will not be available to use.
        let _ = self
            .mknod_and_own_dev(DEV_URANDOM_WITH_NUL, DEV_URANDOM_MAJOR, DEV_URANDOM_MINOR)
            .map_err(|err| {
                println!(
                    "Warning! Could not create /dev/urandom device inside jailer: {}.",
                    err
                );
                println!("MMDS version 2 will not be available to use.");
            });

        // Create requested macvtap devices inside the jailer.
        for iface in &macvtaps {
            let path = Path::new("/dev/net").join(&iface.if_name);
            iface
                .mknod(&path, self.uid, self.gid)
                .map_err(|e| Error::MacVTapMknod(path, e))?
        }

        // Daemonize before exec, if so required (when the dev_null variable != None).
        if let Some(dev_null) = dev_null {
            // Call setsid().
            // SAFETY: Safe because it's a library function.
            SyscallReturnCode(unsafe { libc::setsid() })
                .into_empty_result()
                .map_err(Error::SetSid)?;

            // Replace the stdio file descriptors with the /dev/null fd.
            dup2(dev_null.as_raw_fd(), STDIN_FILENO)?;
            dup2(dev_null.as_raw_fd(), STDOUT_FILENO)?;
            dup2(dev_null.as_raw_fd(), STDERR_FILENO)?;
        }

        // If specified, exec the provided binary into a new PID namespace.
        if self.new_pid_ns {
            self.exec_into_new_pid_ns(chroot_exec_file)
        } else {
            Err(Error::Exec(self.exec_command(chroot_exec_file)))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]

    use std::os::linux::fs::MetadataExt;
    use std::os::unix::ffi::OsStrExt;

    use utils::tempdir::TempDir;
    use utils::tempfile::TempFile;

    use super::*;
    use crate::build_arg_parser;
    use crate::cgroup::test_util::MockCgroupFs;

    const PSEUDO_EXEC_FILE_PATH: &str = "/tmp/pseudo_firecracker_exec_file";

    #[derive(Clone)]
    struct ArgVals<'a> {
        pub id: &'a str,
        pub exec_file: &'a str,
        pub uid: &'a str,
        pub gid: &'a str,
        pub chroot_base: &'a str,
        pub netns: Option<&'a str>,
        pub daemonize: bool,
        pub new_pid_ns: bool,
        pub cgroups: Vec<&'a str>,
        pub resource_limits: Vec<&'a str>,
        pub parent_cgroup: Option<&'a str>,
        pub macvtaps: Vec<&'a str>,
    }

    impl ArgVals<'_> {
        pub fn new() -> ArgVals<'static> {
            File::create(PSEUDO_EXEC_FILE_PATH).unwrap();
            ArgVals {
                id: "bd65600d-8669-4903-8a14-af88203add38",
                exec_file: PSEUDO_EXEC_FILE_PATH,
                uid: "1001",
                gid: "1002",
                chroot_base: "/",
                netns: Some("zzzns"),
                daemonize: true,
                new_pid_ns: true,
                cgroups: vec!["cpu.shares=2", "cpuset.mems=0"],
                resource_limits: vec!["no-file=1024", "fsize=1048575"],
                parent_cgroup: None,
                macvtaps: vec![],
            }
        }
    }

    fn make_args(arg_vals: &ArgVals) -> Vec<String> {
        let mut arg_vec = vec![
            "--binary-name",
            "--id",
            arg_vals.id,
            "--exec-file",
            arg_vals.exec_file,
            "--uid",
            arg_vals.uid,
            "--gid",
            arg_vals.gid,
            "--chroot-base-dir",
            arg_vals.chroot_base,
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<String>>();

        // Append cgroups arguments
        for cg in &arg_vals.cgroups {
            arg_vec.push("--cgroup".to_string());
            arg_vec.push((*cg).to_string());
        }

        // Append limits arguments
        for limit in &arg_vals.resource_limits {
            arg_vec.push("--resource-limit".to_string());
            arg_vec.push((*limit).to_string());
        }
        // Append cgroups arguments
        for macvtap in &arg_vals.macvtaps {
            arg_vec.push("--macvtap".to_string());
            arg_vec.push((*macvtap).to_string());
        }

        if let Some(s) = arg_vals.netns {
            arg_vec.push("--netns".to_string());
            arg_vec.push(s.to_string());
        }

        if arg_vals.daemonize {
            arg_vec.push("--daemonize".to_string());
        }

        if arg_vals.new_pid_ns {
            arg_vec.push("--new-pid-ns".to_string());
        }

        if let Some(parent_cg) = arg_vals.parent_cgroup {
            arg_vec.push("--parent-cgroup".to_string());
            arg_vec.push(parent_cg.to_string());
        }

        arg_vec
    }

    fn get_major(dev: u64) -> u32 {
        unsafe { libc::major(dev) }
    }

    fn get_minor(dev: u64) -> u32 {
        unsafe { libc::minor(dev) }
    }

    fn create_env() -> Env {
        // Create a standard environment.
        let arg_parser = build_arg_parser();
        let mut args = arg_parser.arguments().clone();
        args.parse(&make_args(&ArgVals::new())).unwrap();
        Env::new(&args, 0, 0).unwrap()
    }

    #[test]
    fn test_new_env() {
        let mut mock_cgroups = MockCgroupFs::new().unwrap();
        assert!(mock_cgroups.add_v1_mounts().is_ok());

        let good_arg_vals = ArgVals::new();
        let arg_parser = build_arg_parser();
        let mut args = arg_parser.arguments().clone();
        args.parse(&make_args(&good_arg_vals)).unwrap();
        // This should be fine.
        let good_env =
            Env::new(&args, 0, 0).expect("This new environment should be created successfully.");

        let mut chroot_dir = PathBuf::from(good_arg_vals.chroot_base);
        chroot_dir.push(Path::new(good_arg_vals.exec_file).file_name().unwrap());
        chroot_dir.push(good_arg_vals.id);
        chroot_dir.push("root");

        assert_eq!(good_env.chroot_dir(), chroot_dir);
        assert_eq!(format!("{}", good_env.gid()), good_arg_vals.gid);
        assert_eq!(format!("{}", good_env.uid()), good_arg_vals.uid);

        assert_eq!(good_env.netns, good_arg_vals.netns.map(String::from));
        assert!(good_env.daemonize);
        assert!(good_env.new_pid_ns);

        let another_good_arg_vals = ArgVals {
            netns: None,
            daemonize: false,
            new_pid_ns: false,
            ..good_arg_vals.clone()
        };

        let arg_parser = build_arg_parser();
        args = arg_parser.arguments().clone();
        args.parse(&make_args(&another_good_arg_vals)).unwrap();
        let another_good_env = Env::new(&args, 0, 0)
            .expect("This another new environment should be created successfully.");
        assert!(!another_good_env.daemonize);
        assert!(!another_good_env.new_pid_ns);

        let base_invalid_arg_vals = ArgVals {
            daemonize: true,
            ..another_good_arg_vals.clone()
        };

        let invalid_cgroup_arg_vals = ArgVals {
            cgroups: vec!["zzz"],
            ..base_invalid_arg_vals.clone()
        };

        let arg_parser = build_arg_parser();
        args = arg_parser.arguments().clone();
        args.parse(&make_args(&invalid_cgroup_arg_vals)).unwrap();
        assert!(Env::new(&args, 0, 0).is_err());

        let invalid_res_limit_arg_vals = ArgVals {
            resource_limits: vec!["zzz"],
            ..base_invalid_arg_vals.clone()
        };

        let arg_parser = build_arg_parser();
        args = arg_parser.arguments().clone();
        args.parse(&make_args(&invalid_res_limit_arg_vals)).unwrap();
        assert!(Env::new(&args, 0, 0).is_err());

        let invalid_id_arg_vals = ArgVals {
            id: "/ad./sa12",
            ..base_invalid_arg_vals.clone()
        };

        let arg_parser = build_arg_parser();
        args = arg_parser.arguments().clone();
        args.parse(&make_args(&invalid_id_arg_vals)).unwrap();
        assert!(Env::new(&args, 0, 0).is_err());

        let inexistent_exec_file_arg_vals = ArgVals {
            exec_file: "/this!/file!/should!/not!/exist!/",
            ..base_invalid_arg_vals.clone()
        };

        let arg_parser = build_arg_parser();
        args = arg_parser.arguments().clone();
        args.parse(&make_args(&inexistent_exec_file_arg_vals))
            .unwrap();
        assert!(Env::new(&args, 0, 0).is_err());

        let invalid_uid_arg_vals = ArgVals {
            uid: "zzz",
            ..base_invalid_arg_vals.clone()
        };

        let arg_parser = build_arg_parser();
        args = arg_parser.arguments().clone();
        args.parse(&make_args(&invalid_uid_arg_vals)).unwrap();
        assert!(Env::new(&args, 0, 0).is_err());

        let invalid_gid_arg_vals = ArgVals {
            gid: "zzz",
            ..base_invalid_arg_vals.clone()
        };

        let arg_parser = build_arg_parser();
        args = arg_parser.arguments().clone();
        args.parse(&make_args(&invalid_gid_arg_vals)).unwrap();
        assert!(Env::new(&args, 0, 0).is_err());

        let invalid_parent_cg_vals = ArgVals {
            parent_cgroup: Some("/root"),
            ..base_invalid_arg_vals.clone()
        };
        let macvtap_args = ArgVals {
            macvtaps: vec!["vtap1", "vtap0"],
            ..good_arg_vals.clone()
        };

        let arg_parser = build_arg_parser();
        args = arg_parser.arguments().clone();
        args.parse(&make_args(&invalid_parent_cg_vals)).unwrap();
        assert!(Env::new(&args, 0, 0).is_err());

        let invalid_controller_pt = ArgVals {
            cgroups: vec!["../file_name=1", "./root=1", "/home=1"],
            ..another_good_arg_vals.clone()
        };
        let arg_parser = build_arg_parser();
        args = arg_parser.arguments().clone();
        args.parse(&make_args(&invalid_controller_pt)).unwrap();
        assert!(Env::new(&args, 0, 0).is_err());

        let invalid_format = ArgVals {
            cgroups: vec!["./root/", "../root"],
            ..another_good_arg_vals.clone()
        };
        let arg_parser = build_arg_parser();
        args = arg_parser.arguments().clone();
        args.parse(&make_args(&invalid_format)).unwrap();
        assert!(Env::new(&args, 0, 0).is_err());
        args.parse(&make_args(&macvtap_args)).unwrap();
        assert!(Env::new(&args, 0, 0).is_ok());

        // The chroot-base-dir param is not validated by Env::new, but rather in run, when we
        // actually attempt to create the folder structure (the same goes for netns).
    }

    #[test]
    fn test_dup2() {
        // Open /dev/kvm since it should be available anyway.
        let file1 = fs::File::open("/dev/kvm").unwrap();
        // We open a second file to make sure its associated fd is not used by something else.
        let file2 = fs::File::open("/dev/kvm").unwrap();

        dup2(file1.as_raw_fd(), file2.as_raw_fd()).unwrap();
    }

    #[test]
    fn test_validate_exec_file() {
        // Success case
        File::create(PSEUDO_EXEC_FILE_PATH).unwrap();
        assert!(Env::validate_exec_file(PSEUDO_EXEC_FILE_PATH).is_ok());

        // Error case 1: No such file exists
        std::fs::remove_file(PSEUDO_EXEC_FILE_PATH).unwrap();
        assert_eq!(
            format!(
                "{}",
                Env::validate_exec_file(PSEUDO_EXEC_FILE_PATH).unwrap_err()
            ),
            format!(
                "Failed to canonicalize path {}: No such file or directory (os error 2)",
                PSEUDO_EXEC_FILE_PATH
            )
        );

        // Error case 2: Not a file
        std::fs::create_dir_all("/tmp/firecracker_test_dir").unwrap();
        assert_eq!(
            format!(
                "{}",
                Env::validate_exec_file("/tmp/firecracker_test_dir").unwrap_err()
            ),
            "/tmp/firecracker_test_dir is not a file"
        );

        // Error case 3: Filename without "firecracker"
        File::create("/tmp/firecracker_test_dir/foobarbaz").unwrap();
        assert_eq!(
            format!(
                "{}",
                Env::validate_exec_file("/tmp/firecracker_test_dir/foobarbaz").unwrap_err()
            ),
            "Invalid filename. The filename of `--exec-file` option must contain \"firecracker\": \
             foobarbaz"
        );
        std::fs::remove_file("/tmp/firecracker_test_dir/foobarbaz").unwrap();
        std::fs::remove_dir_all("/tmp/firecracker_test_dir").unwrap();
    }

    #[test]
    fn test_setup_jailed_folder() {
        let mut mock_cgroups = MockCgroupFs::new().unwrap();
        assert!(mock_cgroups.add_v1_mounts().is_ok());
        let env = create_env();

        // Error case: non UTF-8 paths.
        let bad_string: &[u8] = &[0, 102, 111, 111, 0]; // A leading nul followed by 'f', 'o', 'o'
        assert_eq!(
            format!("{}", env.setup_jailed_folder(bad_string).err().unwrap()),
            "Failed to decode string from byte array: data provided contains an interior nul byte \
             at byte pos 0"
        );

        // Error case: inaccessible path - can't be triggered with unit tests running as root.
        // assert_eq!(
        //     format!("{}", env.setup_jailed_folders(vec!["/foo/bar"]).err().unwrap()),
        //     "Failed to create directory /foo/bar: Permission denied (os error 13)"
        // );

        // Success case.
        let foo_dir = TempDir::new().unwrap();
        let mut foo_path = foo_dir.as_path().as_os_str().as_bytes().to_vec();
        foo_path.push(0);
        foo_dir.remove().unwrap();
        assert!(env.setup_jailed_folder(foo_path.as_slice()).is_ok());

        let metadata = fs::metadata(
            CStr::from_bytes_with_nul(foo_path.as_slice())
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap();
        // The mode bits will also have S_IFDIR set because the path belongs to a directory.
        assert_eq!(
            metadata.permissions().mode(),
            FOLDER_PERMISSIONS | libc::S_IFDIR
        );
        assert_eq!(metadata.st_uid(), env.uid);
        assert_eq!(metadata.st_gid(), env.gid);

        // Can't safely test that permissions remain unchanged by umask settings without affecting
        // the umask of the whole unit test process.
        // This crate produces a binary, so Rust integ tests aren't an option either.
        // And changing the umask in the Python integration tests is unsafe because of pytest's
        // process management; it can't be isolated from side effects.
    }

    #[test]
    fn test_mknod_and_own_dev() {
        use std::os::unix::fs::FileTypeExt;

        let mut mock_cgroups = MockCgroupFs::new().unwrap();
        assert!(mock_cgroups.add_v1_mounts().is_ok());
        let env = create_env();

        // Ensure path buffers without NULL-termination are handled well.
        assert!(env.mknod_and_own_dev(b"/some/path", 0, 0).is_err());

        // Ensure device nodes are created with correct major/minor numbers and permissions.
        let dev_infos: Vec<(&[u8], u32, u32)> = vec![
            (b"/dev/net/tun-test\0", DEV_NET_TUN_MAJOR, DEV_NET_TUN_MINOR),
            (b"/dev/kvm-test\0", DEV_KVM_MAJOR, DEV_KVM_MINOR),
        ];

        for (dev, major, minor) in dev_infos {
            let dev_str = CStr::from_bytes_with_nul(dev).unwrap().to_str().unwrap();

            // Checking this just to be super sure there's no file at `dev_str` path (though
            // it shouldn't be as we deleted it at the end of the previous test run).
            if Path::new(dev_str).exists() {
                fs::remove_file(dev_str).unwrap();
            }

            // Create a new device node.
            env.mknod_and_own_dev(dev, major, minor).unwrap();

            // Ensure device's properties.
            let metadata = fs::metadata(dev_str).unwrap();
            assert!(metadata.file_type().is_char_device());
            assert_eq!(get_major(metadata.st_rdev()), major);
            assert_eq!(get_minor(metadata.st_rdev()), minor);
            assert_eq!(
                metadata.permissions().mode(),
                libc::S_IFCHR | libc::S_IRUSR | libc::S_IWUSR
            );

            // Trying to create again the same device node is not allowed.
            assert_eq!(
                format!("{}", env.mknod_and_own_dev(dev, major, minor).unwrap_err()),
                format!(
                    "Failed to create {}\u{0} via mknod inside the jail: File exists (os error 17)",
                    dev_str
                )
            );
            // Remove the device node.
            fs::remove_file(dev_str).expect("Could not remove file.");
        }
    }

    #[test]
    fn test_copy_exec_to_chroot() {
        // Create a standard environment.
        let arg_parser = build_arg_parser();
        let mut args = arg_parser.arguments().clone();
        let mut mock_cgroups = MockCgroupFs::new().unwrap();
        assert!(mock_cgroups.add_v1_mounts().is_ok());

        // Create tmp resources for `exec_file` and `chroot_base`.
        File::create(PSEUDO_EXEC_FILE_PATH).unwrap();
        let exec_file_path = PSEUDO_EXEC_FILE_PATH;
        let exec_file_name = Path::new(exec_file_path).file_name().unwrap();
        let some_dir = TempDir::new().unwrap();
        let some_dir_path = some_dir.as_path().to_str().unwrap();

        let some_arg_vals = ArgVals {
            id: "bd65600d-8669-4903-8a14-af88203add38",
            exec_file: exec_file_path,
            uid: "1001",
            gid: "1002",
            chroot_base: some_dir_path,
            netns: Some("zzzns"),
            daemonize: false,
            new_pid_ns: false,
            cgroups: Vec::new(),
            resource_limits: Vec::new(),
            parent_cgroup: None,
            macvtaps: Vec::new(),
        };
        fs::write(exec_file_path, "some_content").unwrap();
        args.parse(&make_args(&some_arg_vals)).unwrap();
        let mut env = Env::new(&args, 0, 0).unwrap();

        // Create the required chroot dir hierarchy.
        fs::create_dir_all(env.chroot_dir()).expect("Could not create dir hierarchy.");

        assert_eq!(
            env.copy_exec_to_chroot().unwrap(),
            exec_file_name.to_os_string()
        );

        let dest_path = env.chroot_dir.join(exec_file_name);
        // Check that `fs::copy()` copied src content and permission bits to destination.
        let metadata_src = fs::metadata(&env.exec_file_path).unwrap();
        let metadata_dest = fs::metadata(&dest_path).unwrap();
        let content_src = fs::read(&env.exec_file_path).unwrap();
        let content_dest = fs::read(&dest_path).unwrap();
        assert_eq!(content_src, content_dest);
        assert_eq!(content_dest, b"some_content");
        assert_eq!(metadata_src.permissions(), metadata_dest.permissions());

        // Clean up the environment.
        fs::remove_dir_all(env.chroot_dir()).expect("Could not remove dir hierarchy.");
    }

    #[test]
    fn test_join_netns() {
        let mut path = "invalid_path";
        assert_eq!(
            format!("{}", Env::join_netns(path).unwrap_err()),
            format!(
                "Failed to open file {}: No such file or directory (os error 2)",
                path
            )
        );

        let tmp_file = TempFile::new().unwrap();
        path = tmp_file.as_path().to_str().unwrap();
        assert_eq!(
            format!("{}", Env::join_netns(path).unwrap_err()),
            "Failed to join network namespace: netns: Invalid argument (os error 22)"
        );

        // Testing `join_netns()` with a valid network namespace is not that easy
        // as Rust std library doesn't offer support for creating such namespaces.
    }

    #[test]
    fn test_cgroups_parsing() {
        let arg_parser = build_arg_parser();
        let good_arg_vals = ArgVals::new();
        let mut mock_cgroups = MockCgroupFs::new().unwrap();
        assert!(mock_cgroups.add_v1_mounts().is_ok());

        // Cases that should fail

        // Check string without "." (no controller)
        let mut args = arg_parser.arguments().clone();
        let invalid_cgroup_arg_vals = ArgVals {
            cgroups: vec!["cpusetcpus=2"],
            ..good_arg_vals.clone()
        };
        args.parse(&make_args(&invalid_cgroup_arg_vals)).unwrap();
        assert!(Env::new(&args, 0, 0).is_err());

        // Check empty string
        let mut args = arg_parser.arguments().clone();
        let invalid_cgroup_arg_vals = ArgVals {
            cgroups: vec![""],
            ..good_arg_vals.clone()
        };
        args.parse(&make_args(&invalid_cgroup_arg_vals)).unwrap();
        assert!(Env::new(&args, 0, 0).is_err());

        // Check valid file empty value
        let mut args = arg_parser.arguments().clone();
        let invalid_cgroup_arg_vals = ArgVals {
            cgroups: vec!["cpuset.cpus="],
            ..good_arg_vals.clone()
        };
        args.parse(&make_args(&invalid_cgroup_arg_vals)).unwrap();
        assert!(Env::new(&args, 0, 0).is_err());

        // Check valid file no value
        let mut args = arg_parser.arguments().clone();
        let invalid_cgroup_arg_vals = ArgVals {
            cgroups: vec!["cpuset.cpus"],
            ..good_arg_vals.clone()
        };
        args.parse(&make_args(&invalid_cgroup_arg_vals)).unwrap();
        assert!(Env::new(&args, 0, 0).is_err());

        // Cases that should succeed

        // Check value with special characters (',', '.', '-')
        let mut args = arg_parser.arguments().clone();
        let invalid_cgroup_arg_vals = ArgVals {
            cgroups: vec!["cpuset.cpus=2-4,5.3"],
            ..good_arg_vals.clone()
        };
        args.parse(&make_args(&invalid_cgroup_arg_vals)).unwrap();
        assert!(Env::new(&args, 0, 0).is_ok());

        // Check valid case
        let mut args = arg_parser.arguments().clone();
        let invalid_cgroup_arg_vals = ArgVals {
            cgroups: vec!["cpuset.cpus=2"],
            ..good_arg_vals.clone()
        };
        args.parse(&make_args(&invalid_cgroup_arg_vals)).unwrap();
        assert!(Env::new(&args, 0, 0).is_ok());

        // Check file with multiple "."
        let mut args = arg_parser.arguments().clone();
        let invalid_cgroup_arg_vals = ArgVals {
            cgroups: vec!["memory.swap.high=2"],
            ..good_arg_vals.clone()
        };
        args.parse(&make_args(&invalid_cgroup_arg_vals)).unwrap();
        assert!(Env::new(&args, 0, 0).is_ok());
    }

    #[test]
    fn test_parse_resource_limits() {
        let mut resource_limits = ResourceLimits::default();

        // Cases that should fail

        // Check invalid formats
        let invalid_formats = ["", "foo"];
        for format in invalid_formats.iter() {
            let arg = vec![format.to_string()];
            assert_eq!(
                format!(
                    "{:?}",
                    Env::parse_resource_limits(&mut resource_limits, &*arg)
                        .err()
                        .unwrap()
                ),
                format!("{:?}", Error::ResLimitFormat(format.to_string()))
            );
        }

        // Check invalid resource arguments
        let invalid_resources = ["foo", "", " "];
        for res in invalid_resources.iter() {
            let arg = format!("{}=2", res);
            assert_eq!(
                format!(
                    "{:?}",
                    Env::parse_resource_limits(&mut resource_limits, &*vec![arg])
                        .err()
                        .unwrap()
                ),
                format!("{:?}", Error::ResLimitArgument(res.to_string()))
            );
        }

        // Check invalid limit values
        let invalid_values = ["foo", "2.3", "2-3", " "];
        for val in invalid_values.iter() {
            let arg = format!("fsize={}", val);
            assert_eq!(
                format!(
                    "{:?}",
                    Env::parse_resource_limits(&mut resource_limits, &*vec![arg])
                        .err()
                        .unwrap()
                ),
                format!(
                    "{:?}",
                    Error::ResLimitValue(
                        val.to_string(),
                        "invalid digit found in string".to_string()
                    )
                )
            );
        }

        // Check valid cases
        let resources = [FSIZE_ARG, NO_FILE_ARG];
        for resource in resources.iter() {
            let arg = vec![resource.to_string() + "=4098"];
            Env::parse_resource_limits(&mut resource_limits, &*arg).unwrap();
        }
    }

    #[test]
    #[cfg(target_arch = "aarch64")]
    fn test_copy_cache_info() {
        let mut mock_cgroups = MockCgroupFs::new().unwrap();
        assert!(mock_cgroups.add_v1_mounts().is_ok());

        let env = create_env();

        // Create the required chroot dir hierarchy.
        fs::create_dir_all(env.chroot_dir()).expect("Could not create dir hierarchy.");

        assert!(env.copy_cache_info().is_ok());

        // Make sure that the needed files truly exist.
        const JAILER_CACHE_INFO: &str = "sys/devices/system/cpu/cpu0/cache";

        let dest_path = env.chroot_dir.join(JAILER_CACHE_INFO);
        assert!(fs::metadata(&dest_path).is_ok());
        let index_dest_path = dest_path.join("index0");
        assert!(fs::metadata(&index_dest_path).is_ok());
        let entries = fs::read_dir(&index_dest_path).unwrap();
        assert_eq!(entries.enumerate().count(), 6);
    }

    #[test]
    fn test_save_exec_file_pid() {
        let exec_file_name = "file";
        let pid_file_name = "file.pid";
        let pid = 1;

        let mut mock_cgroups = MockCgroupFs::new().unwrap();
        assert!(mock_cgroups.add_v1_mounts().is_ok());

        let mut env = create_env();
        env.save_exec_file_pid(pid, PathBuf::from(exec_file_name))
            .unwrap();

        let stored_pid = fs::read_to_string(pid_file_name);
        fs::remove_file(pid_file_name).unwrap();
        assert_eq!(stored_pid.unwrap(), "1");
    }
}
