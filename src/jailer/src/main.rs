// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#![warn(clippy::ptr_as_ptr)]
#![warn(clippy::undocumented_unsafe_blocks)]
#![warn(clippy::cast_lossless)]

mod cgroup;
mod chroot;
mod env;
mod resource_limits;
use std::ffi::{CString, NulError, OsString};
use std::path::{Path, PathBuf};
use std::{env as p_env, fmt, fs, io, process, result};

use utils::arg_parser::{ArgParser, Argument, Error as ParsingError};
use utils::validators;

use crate::env::Env;

const JAILER_VERSION: &str = env!("FIRECRACKER_VERSION");
#[derive(Debug)]
pub enum Error {
    ArgumentParsing(ParsingError),
    Canonicalize(PathBuf, io::Error),
    CgroupInheritFromParent(PathBuf, String),
    CgroupLineNotFound(String, String),
    CgroupInvalidFile(String),
    CgroupWrite(String, String, String),
    CgroupFormat(String),
    CgroupHierarchyMissing(String),
    CgroupControllerUnavailable(String),
    CgroupInvalidVersion(String),
    CgroupInvalidParentPath(),
    ChangeFileOwner(PathBuf, io::Error),
    ChdirNewRoot(io::Error),
    Chmod(PathBuf, io::Error),
    Clone(io::Error),
    CloseNetNsFd(io::Error),
    CloseDevNullFd(io::Error),
    Copy(PathBuf, PathBuf, io::Error),
    CreateDir(PathBuf, io::Error),
    CStringParsing(NulError),
    Dup2(io::Error),
    Exec(io::Error),
    ExecFileName(String),
    ExtractFileName(PathBuf),
    FileOpen(PathBuf, io::Error),
    FromBytesWithNul(std::ffi::FromBytesWithNulError),
    GetOldFdFlags(io::Error),
    Gid(String),
    InvalidInstanceId(validators::Error),
    MacVTapByName(String, io::Error),
    MacVTapMknod(PathBuf, io::Error),
    MissingParent(PathBuf),
    MkdirOldRoot(io::Error),
    MknodDev(io::Error, &'static str),
    MountBind(io::Error),
    MountPropagationSlave(io::Error),
    MountSysfs(io::Error),
    NotAFile(PathBuf),
    NotADirectory(PathBuf),
    OpenDevNull(io::Error),
    OsStringParsing(PathBuf, OsString),
    PivotRoot(io::Error),
    ReadLine(PathBuf, io::Error),
    ReadToString(PathBuf, io::Error),
    RegEx(regex::Error),
    ResLimitArgument(String),
    ResLimitFormat(String),
    ResLimitValue(String, String),
    RmOldRootDir(io::Error),
    SetCurrentDir(io::Error),
    SetNetNs(io::Error),
    Setrlimit(String),
    SetSid(io::Error),
    Uid(String),
    UmountOldRoot(io::Error),
    UmountSysfs(io::Error),
    UnexpectedListenerFd(i32),
    UnshareNewNs(io::Error),
    UnsetCloexec(io::Error),
    Write(PathBuf, io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match *self {
            ArgumentParsing(ref err) => write!(f, "Failed to parse arguments: {}", err),
            Canonicalize(ref path, ref io_err) => write!(
                f,
                "{}",
                format!("Failed to canonicalize path {:?}: {}", path, io_err).replace('\"', "")
            ),
            Chmod(ref path, ref err) => {
                write!(f, "Failed to change permissions on {:?}: {}", path, err)
            }
            CgroupInheritFromParent(ref path, ref filename) => write!(
                f,
                "{}",
                format!(
                    "Failed to inherit cgroups configurations from file {} in path {:?}",
                    filename, path
                )
                .replace('\"', "")
            ),
            CgroupLineNotFound(ref proc_mounts, ref controller) => write!(
                f,
                "{} configurations not found in {}",
                controller, proc_mounts
            ),
            CgroupInvalidFile(ref file) => write!(f, "Cgroup invalid file: {}", file,),
            CgroupWrite(ref evalue, ref rvalue, ref file) => write!(
                f,
                "Expected value {} for {}. Current value: {}",
                evalue, file, rvalue
            ),
            CgroupFormat(ref arg) => write!(f, "Invalid format for cgroups: {}", arg,),
            CgroupHierarchyMissing(ref arg) => write!(f, "Hierarchy not found: {}", arg,),
            CgroupControllerUnavailable(ref arg) => write!(f, "Controller {} is unavailable", arg,),
            CgroupInvalidVersion(ref arg) => {
                write!(f, "{} is an invalid cgroup version specifier", arg,)
            }
            CgroupInvalidParentPath() => {
                write!(
                    f,
                    "Parent cgroup path is invalid. Path should not be absolute or contain '..' \
                     or '.'",
                )
            }
            ChangeFileOwner(ref path, ref err) => {
                write!(f, "Failed to change owner for {:?}: {}", path, err)
            }
            ChdirNewRoot(ref err) => write!(f, "Failed to chdir into chroot directory: {}", err),
            Clone(ref err) => write!(f, "Failed cloning into a new child process: {}", err),
            CloseNetNsFd(ref err) => write!(f, "Failed to close netns fd: {}", err),
            CloseDevNullFd(ref err) => write!(f, "Failed to close /dev/null fd: {}", err),
            Copy(ref file, ref path, ref err) => write!(
                f,
                "{}",
                format!("Failed to copy {:?} to {:?}: {}", file, path, err).replace('\"', "")
            ),
            CreateDir(ref path, ref err) => write!(
                f,
                "{}",
                format!("Failed to create directory {:?}: {}", path, err).replace('\"', "")
            ),
            CStringParsing(_) => write!(f, "Encountered interior \\0 while parsing a string"),
            Dup2(ref err) => write!(f, "Failed to duplicate fd: {}", err),
            Exec(ref err) => write!(f, "Failed to exec into Firecracker: {}", err),
            ExecFileName(ref filename) => write!(
                f,
                "Invalid filename. The filename of `--exec-file` option must contain \
                 \"firecracker\": {}",
                filename
            ),
            ExtractFileName(ref path) => write!(
                f,
                "{}",
                format!("Failed to extract filename from path {:?}", path).replace('\"', "")
            ),
            FileOpen(ref path, ref err) => write!(
                f,
                "{}",
                format!("Failed to open file {:?}: {}", path, err).replace('\"', "")
            ),
            FromBytesWithNul(ref err) => {
                write!(f, "Failed to decode string from byte array: {}", err)
            }
            GetOldFdFlags(ref err) => write!(f, "Failed to get flags from fd: {}", err),
            Gid(ref gid) => write!(f, "Invalid gid: {}", gid),
            InvalidInstanceId(ref err) => write!(f, "Invalid instance ID: {}", err),
            MacVTapByName(ref name, ref err) => {
                write!(f, "Failed to resolve macvtap interface {}: {}", name, err)
            }
            MacVTapMknod(ref path, ref err) => write!(
                f,
                "{}",
                format!(
                    "Failed to create {:?} via mknod inside the jail: {}",
                    path, err
                )
                .replace("\"", "")
            ),
            MissingParent(ref path) => write!(
                f,
                "{}",
                format!("File {:?} doesn't have a parent", path).replace('\"', "")
            ),
            MkdirOldRoot(ref err) => write!(
                f,
                "Failed to create the jail root directory before pivoting root: {}",
                err
            ),
            MknodDev(ref err, ref devname) => write!(
                f,
                "Failed to create {} via mknod inside the jail: {}",
                devname, err
            ),
            MountBind(ref err) => {
                write!(f, "Failed to bind mount the jail root directory: {}", err)
            }
            MountSysfs(ref err) => {
                write!(f, "Failed to mount sysfs for network namespace: {}", err)
            }
            MountPropagationSlave(ref err) => {
                write!(f, "Failed to change the propagation type to slave: {}", err)
            }
            NotAFile(ref path) => write!(
                f,
                "{}",
                format!("{:?} is not a file", path).replace('\"', "")
            ),
            NotADirectory(ref path) => write!(
                f,
                "{}",
                format!("{:?} is not a directory", path).replace('\"', "")
            ),
            OpenDevNull(ref err) => write!(f, "Failed to open /dev/null: {}", err),
            OsStringParsing(ref path, _) => write!(
                f,
                "{}",
                format!("Failed to parse path {:?} into an OsString", path).replace('\"', "")
            ),
            PivotRoot(ref err) => write!(f, "Failed to pivot root: {}", err),
            ReadLine(ref path, ref err) => write!(
                f,
                "{}",
                format!("Failed to read line from {:?}: {}", path, err).replace('\"', "")
            ),
            ReadToString(ref path, ref err) => write!(
                f,
                "{}",
                format!("Failed to read file {:?} into a string: {}", path, err).replace('\"', "")
            ),
            RegEx(ref err) => write!(f, "Regex failed: {:?}", err),
            ResLimitArgument(ref arg) => write!(f, "Invalid resource argument: {}", arg,),
            ResLimitFormat(ref arg) => write!(f, "Invalid format for resources limits: {}", arg,),
            ResLimitValue(ref arg, ref err) => {
                write!(f, "Invalid limit value for resource: {}: {}", arg, err)
            }
            RmOldRootDir(ref err) => write!(f, "Failed to remove old jail root directory: {}", err),
            SetCurrentDir(ref err) => write!(f, "Failed to change current directory: {}", err),
            SetNetNs(ref err) => write!(f, "Failed to join network namespace: netns: {}", err),
            Setrlimit(ref err) => write!(f, "Failed to set limit for resource: {}", err),
            SetSid(ref err) => write!(f, "Failed to daemonize: setsid: {}", err),
            Uid(ref uid) => write!(f, "Invalid uid: {}", uid),
            UmountOldRoot(ref err) => write!(f, "Failed to unmount the old jail root: {}", err),
            UmountSysfs(ref err) => {
                write!(f, "Failed to unmount sysfs for network namespace: {}", err)
            }
            UnexpectedListenerFd(fd) => {
                write!(f, "Unexpected value for the socket listener fd: {}", fd)
            }
            UnshareNewNs(ref err) => {
                write!(f, "Failed to unshare into new mount namespace: {}", err)
            }
            UnsetCloexec(ref err) => write!(
                f,
                "Failed to unset the O_CLOEXEC flag on the socket fd: {}",
                err
            ),
            Write(ref path, ref err) => write!(
                f,
                "{}",
                format!("Failed to write to {:?}: {}", path, err).replace('\"', "")
            ),
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

/// Create an ArgParser object which contains info about the command line argument parser and
/// populate it with the expected arguments and their characteristics.
pub fn build_arg_parser() -> ArgParser<'static> {
    ArgParser::new()
        .arg(
            Argument::new("id")
                .required(true)
                .takes_value(true)
                .help("Jail ID."),
        )
        .arg(
            Argument::new("exec-file")
                .required(true)
                .takes_value(true)
                .help("File path to exec into."),
        )
        .arg(
            Argument::new("uid")
                .required(true)
                .takes_value(true)
                .help("The user identifier the jailer switches to after exec."),
        )
        .arg(
            Argument::new("gid")
                .required(true)
                .takes_value(true)
                .help("The group identifier the jailer switches to after exec."),
        )
        .arg(
            Argument::new("chroot-base-dir")
                .takes_value(true)
                .default_value("/srv/jailer")
                .help("The base folder where chroot jails are located."),
        )
        .arg(
            Argument::new("netns")
                .takes_value(true)
                .help("Path to the network namespace this microVM should join."),
        )
        .arg(Argument::new("daemonize").takes_value(false).help(
            "Daemonize the jailer before exec, by invoking setsid(), and redirecting the standard \
             I/O file descriptors to /dev/null.",
        ))
        .arg(
            Argument::new("new-pid-ns")
                .takes_value(false)
                .help("Exec into a new PID namespace."),
        )
        .arg(Argument::new("cgroup").allow_multiple(true).help(
            "Cgroup and value to be set by the jailer. It must follow this format: \
             <cgroup_file>=<value> (e.g cpu.shares=10). This argument can be used multiple times \
             to add multiple cgroups.",
        ))
        .arg(Argument::new("resource-limit").allow_multiple(true).help(
            "Resource limit values to be set by the jailer. It must follow this format: \
             <resource>=<value> (e.g no-file=1024). This argument can be used multiple times to \
             add multiple resource limits. Current available resource values are:\n\t\tfsize: The \
             maximum size in bytes for files created by the process.\n\t\tno-file: Specifies a \
             value one greater than the maximum file descriptor number that can be opened by this \
             process.",
        ))
        .arg(
            Argument::new("cgroup-version")
                .takes_value(true)
                .default_value("1")
                .help("Select the cgroup version used by the jailer."),
        )
        .arg(
            Argument::new("parent-cgroup")
                .takes_value(true)
                .help("Parent cgroup in which the cgroup of this microvm will be placed."),
        )
        .arg(
            Argument::new("version")
                .takes_value(false)
                .help("Print the binary version number."),
        )
        .arg(
            Argument::new("macvtap")
                .takes_value(true)
                .allow_multiple(true)
                .help("Name of macvtap interface to make available to the firecracker process."),
        )
}

// It's called writeln_special because we have to use this rather convoluted way of writing
// to special cgroup files, to avoid getting errors. It would be nice to know why that happens :-s
pub fn writeln_special<T, V>(file_path: &T, value: V) -> Result<()>
where
    T: AsRef<Path>,
    V: ::std::fmt::Display,
{
    fs::write(file_path, format!("{}\n", value))
        .map_err(|err| Error::Write(PathBuf::from(file_path.as_ref()), err))
}

pub fn readln_special<T: AsRef<Path>>(file_path: &T) -> Result<String> {
    let mut line = fs::read_to_string(file_path)
        .map_err(|err| Error::ReadToString(PathBuf::from(file_path.as_ref()), err))?;

    // Remove the newline character at the end (if any).
    line.pop();

    Ok(line)
}

fn sanitize_process() {
    // First thing to do is make sure we don't keep any inherited FDs
    // other that IN, OUT and ERR.
    if let Ok(mut paths) = fs::read_dir("/proc/self/fd") {
        while let Some(Ok(path)) = paths.next() {
            let file_name = path.file_name();
            let fd_str = file_name.to_str().unwrap_or("0");
            let fd = fd_str.parse::<i32>().unwrap_or(0);

            if fd > 2 {
                // SAFETY: Safe because close() cannot fail when passed a valid parameter.
                unsafe { libc::close(fd) };
            }
        }
    }

    // Cleanup environment variables
    clean_env_vars();
}

fn clean_env_vars() {
    // Remove environment variables received from
    // the parent process so there are no leaks
    // inside the jailer environment
    for (key, _) in p_env::vars() {
        p_env::remove_var(key);
    }
}

/// Turns an AsRef<Path> into a CString (c style string).
/// The expect should not fail, since Linux paths only contain valid Unicode chars (do they?),
/// and do not contain null bytes (do they?).
pub fn to_cstring<T: AsRef<Path>>(path: T) -> Result<CString> {
    let path_str = path
        .as_ref()
        .to_path_buf()
        .into_os_string()
        .into_string()
        .map_err(|err| Error::OsStringParsing(path.as_ref().to_path_buf(), err))?;
    CString::new(path_str).map_err(Error::CStringParsing)
}

fn main() {
    sanitize_process();

    let mut arg_parser = build_arg_parser();

    match arg_parser.parse_from_cmdline() {
        Err(err) => {
            println!(
                "Arguments parsing error: {} \n\nFor more information try --help.",
                err
            );
            process::exit(1);
        }
        _ => {
            if arg_parser.arguments().flag_present("help") {
                println!("Jailer v{}\n", JAILER_VERSION);
                println!("{}\n", arg_parser.formatted_help());
                println!(
                    "Any arguments after the -- separator will be supplied to the jailed binary.\n"
                );
                process::exit(0);
            }

            if arg_parser.arguments().flag_present("version") {
                println!("Jailer v{}\n", JAILER_VERSION);
                process::exit(0);
            }
        }
    }

    Env::new(
        arg_parser.arguments(),
        utils::time::get_time_us(utils::time::ClockType::Monotonic),
        utils::time::get_time_us(utils::time::ClockType::ProcessCpu),
    )
    .and_then(|env| {
        fs::create_dir_all(env.chroot_dir())
            .map_err(|err| Error::CreateDir(env.chroot_dir().to_owned(), err))?;
        env.run()
    })
    .unwrap_or_else(|err| panic!("Jailer error: {}", err));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]
    use std::env;
    use std::fs::File;
    use std::os::unix::io::IntoRawFd;

    use utils::arg_parser;

    use super::*;

    #[test]
    fn test_sanitize_process() {
        let n = 100;

        let tmp_dir_path = "/tmp/jailer/tests/sanitize_process";
        assert!(fs::create_dir_all(tmp_dir_path).is_ok());

        let mut fds = Vec::new();
        for i in 0..n {
            let maybe_file = File::create(format!("{}/{}", tmp_dir_path, i));
            assert!(maybe_file.is_ok());
            fds.push(maybe_file.unwrap().into_raw_fd());
        }

        sanitize_process();

        for fd in fds {
            let is_fd_opened = unsafe { libc::fcntl(fd, libc::F_GETFD) } == 0;
            assert!(!is_fd_opened);
        }

        assert!(fs::remove_dir_all(tmp_dir_path).is_ok());
    }

    #[test]
    fn test_clean_env_vars() {
        let env_vars: [&str; 5] = ["VAR1", "VAR2", "VAR3", "VAR4", "VAR5"];

        // Set environment variables
        for env_var in env_vars.iter() {
            env::set_var(env_var, "0");
        }

        // Cleanup the environment
        clean_env_vars();

        // Assert that the variables set beforehand
        // do not exist anymore
        for env_var in env_vars.iter() {
            assert_eq!(env::var_os(env_var), None);
        }
    }

    #[allow(clippy::cognitive_complexity)]
    #[test]
    fn test_error_display() {
        use std::ffi::CStr;

        let path = PathBuf::from("/foo");
        let file_str = "/foo/bar";
        let file_path = PathBuf::from(file_str);
        let proc_mounts = "/proc/mounts";
        let controller = "sysfs";
        let id = "foobar";
        let err_args_parse = arg_parser::Error::UnexpectedArgument("foo".to_string());
        let err_regex = regex::Error::Syntax(id.to_string());
        let err2_str = "No such file or directory (os error 2)";
        let cgroup_file = "cpuset.mems";

        assert_eq!(
            format!("{}", Error::ArgumentParsing(err_args_parse)),
            "Failed to parse arguments: Found argument 'foo' which wasn't expected, or isn't \
             valid in this context."
        );
        assert_eq!(
            format!(
                "{}",
                Error::Canonicalize(path.clone(), io::Error::from_raw_os_error(2))
            ),
            format!("Failed to canonicalize path /foo: {}", err2_str)
        );
        assert_eq!(
            format!(
                "{}",
                Error::CgroupInheritFromParent(path.clone(), file_str.to_string())
            ),
            "Failed to inherit cgroups configurations from file /foo/bar in path /foo",
        );
        assert_eq!(
            format!(
                "{}",
                Error::Chmod(path.clone(), io::Error::from_raw_os_error(2))
            ),
            "Failed to change permissions on \"/foo\": No such file or directory (os error 2)",
        );
        assert_eq!(
            format!(
                "{}",
                Error::CgroupLineNotFound(proc_mounts.to_string(), controller.to_string())
            ),
            "sysfs configurations not found in /proc/mounts",
        );
        assert_eq!(
            format!("{}", Error::CgroupInvalidFile(cgroup_file.to_string())),
            "Cgroup invalid file: cpuset.mems",
        );
        assert_eq!(
            format!(
                "{}",
                Error::CgroupWrite("1".to_string(), "2".to_string(), cgroup_file.to_string())
            ),
            "Expected value 1 for cpuset.mems. Current value: 2",
        );
        assert_eq!(
            format!("{}", Error::CgroupFormat(cgroup_file.to_string())),
            "Invalid format for cgroups: cpuset.mems",
        );

        assert_eq!(
            format!(
                "{}",
                Error::ChangeFileOwner(
                    PathBuf::from("/dev/net/tun"),
                    io::Error::from_raw_os_error(42)
                )
            ),
            "Failed to change owner for \"/dev/net/tun\": No message of desired type (os error 42)",
        );
        assert_eq!(
            format!("{}", Error::ChdirNewRoot(io::Error::from_raw_os_error(42))),
            "Failed to chdir into chroot directory: No message of desired type (os error 42)"
        );
        assert_eq!(
            format!("{}", Error::Clone(io::Error::from_raw_os_error(42))),
            "Failed cloning into a new child process: No message of desired type (os error 42)",
        );
        assert_eq!(
            format!("{}", Error::CloseNetNsFd(io::Error::from_raw_os_error(42))),
            "Failed to close netns fd: No message of desired type (os error 42)",
        );
        assert_eq!(
            format!(
                "{}",
                Error::CloseDevNullFd(io::Error::from_raw_os_error(42))
            ),
            "Failed to close /dev/null fd: No message of desired type (os error 42)",
        );
        assert_eq!(
            format!(
                "{}",
                Error::Copy(
                    file_path.clone(),
                    path.clone(),
                    io::Error::from_raw_os_error(2)
                )
            ),
            format!("Failed to copy /foo/bar to /foo: {}", err2_str)
        );
        assert_eq!(
            format!(
                "{}",
                Error::CreateDir(path, io::Error::from_raw_os_error(2))
            ),
            format!("Failed to create directory /foo: {}", err2_str)
        );
        assert_eq!(
            format!(
                "{}",
                Error::CStringParsing(CString::new(b"f\0oo".to_vec()).unwrap_err())
            ),
            "Encountered interior \\0 while parsing a string",
        );
        assert_eq!(
            format!("{}", Error::Dup2(io::Error::from_raw_os_error(42))),
            "Failed to duplicate fd: No message of desired type (os error 42)",
        );
        assert_eq!(
            format!("{}", Error::Exec(io::Error::from_raw_os_error(2))),
            format!("Failed to exec into Firecracker: {}", err2_str)
        );
        assert_eq!(
            format!("{}", Error::ExecFileName("foobarbaz".to_string())),
            "Invalid filename. The filename of `--exec-file` option must contain \"firecracker\": \
             foobarbaz",
        );
        assert_eq!(
            format!("{}", Error::ExtractFileName(file_path.clone())),
            "Failed to extract filename from path /foo/bar",
        );
        assert_eq!(
            format!(
                "{}",
                Error::FileOpen(file_path.clone(), io::Error::from_raw_os_error(2))
            ),
            format!("Failed to open file /foo/bar: {}", err2_str)
        );

        let err = CStr::from_bytes_with_nul(b"/dev").err().unwrap();
        assert_eq!(
            format!("{}", Error::FromBytesWithNul(err)),
            "Failed to decode string from byte array: data provided is not nul terminated",
        );
        assert_eq!(
            format!("{}", Error::GetOldFdFlags(io::Error::from_raw_os_error(42))),
            "Failed to get flags from fd: No message of desired type (os error 42)",
        );
        assert_eq!(
            format!("{}", Error::Gid(id.to_string())),
            "Invalid gid: foobar",
        );
        assert_eq!(
            format!(
                "{}",
                Error::InvalidInstanceId(validators::Error::InvalidChar('a', 1))
            ),
            "Invalid instance ID: invalid char (a) at position 1",
        );
        assert_eq!(
            format!("{}", Error::MissingParent(file_path.clone())),
            "File /foo/bar doesn't have a parent",
        );
        assert_eq!(
            format!("{}", Error::MkdirOldRoot(io::Error::from_raw_os_error(42))),
            "Failed to create the jail root directory before pivoting root: No message of desired \
             type (os error 42)",
        );
        assert_eq!(
            format!(
                "{}",
                Error::MknodDev(io::Error::from_raw_os_error(42), "/dev/net/tun")
            ),
            "Failed to create /dev/net/tun via mknod inside the jail: No message of desired type \
             (os error 42)",
        );
        assert_eq!(
            format!("{}", Error::MountBind(io::Error::from_raw_os_error(42))),
            "Failed to bind mount the jail root directory: No message of desired type (os error \
             42)",
        );
        assert_eq!(
            format!(
                "{}",
                Error::MountPropagationSlave(io::Error::from_raw_os_error(42))
            ),
            "Failed to change the propagation type to slave: No message of desired type (os error \
             42)",
        );
        assert_eq!(
            format!("{}", Error::NotAFile(file_path.clone())),
            "/foo/bar is not a file",
        );
        assert_eq!(
            format!("{}", Error::NotADirectory(file_path.clone())),
            "/foo/bar is not a directory",
        );
        assert_eq!(
            format!("{}", Error::OpenDevNull(io::Error::from_raw_os_error(42))),
            "Failed to open /dev/null: No message of desired type (os error 42)",
        );
        assert_eq!(
            format!(
                "{}",
                Error::OsStringParsing(file_path.clone(), file_path.clone().into_os_string())
            ),
            "Failed to parse path /foo/bar into an OsString",
        );
        assert_eq!(
            format!("{}", Error::PivotRoot(io::Error::from_raw_os_error(42))),
            "Failed to pivot root: No message of desired type (os error 42)",
        );
        assert_eq!(
            format!(
                "{}",
                Error::ReadLine(file_path.clone(), io::Error::from_raw_os_error(2))
            ),
            format!("Failed to read line from /foo/bar: {}", err2_str)
        );
        assert_eq!(
            format!(
                "{}",
                Error::ReadToString(file_path.clone(), io::Error::from_raw_os_error(2))
            ),
            format!("Failed to read file /foo/bar into a string: {}", err2_str)
        );
        assert_eq!(
            format!("{}", Error::RegEx(err_regex.clone())),
            format!("Regex failed: {:?}", err_regex),
        );
        assert_eq!(
            format!("{}", Error::ResLimitArgument("foo".to_string())),
            "Invalid resource argument: foo",
        );
        assert_eq!(
            format!("{}", Error::ResLimitFormat("foo".to_string())),
            "Invalid format for resources limits: foo",
        );
        assert_eq!(
            format!(
                "{}",
                Error::ResLimitValue("foo".to_string(), "bar".to_string())
            ),
            "Invalid limit value for resource: foo: bar",
        );
        assert_eq!(
            format!("{}", Error::RmOldRootDir(io::Error::from_raw_os_error(42))),
            "Failed to remove old jail root directory: No message of desired type (os error 42)",
        );
        assert_eq!(
            format!("{}", Error::SetCurrentDir(io::Error::from_raw_os_error(2))),
            format!("Failed to change current directory: {}", err2_str),
        );
        assert_eq!(
            format!("{}", Error::SetNetNs(io::Error::from_raw_os_error(42))),
            "Failed to join network namespace: netns: No message of desired type (os error 42)",
        );
        assert_eq!(
            format!("{}", Error::Setrlimit("foobar".to_string())),
            "Failed to set limit for resource: foobar",
        );
        assert_eq!(
            format!("{}", Error::SetSid(io::Error::from_raw_os_error(42))),
            "Failed to daemonize: setsid: No message of desired type (os error 42)",
        );
        assert_eq!(
            format!("{}", Error::Uid(id.to_string())),
            "Invalid uid: foobar",
        );
        assert_eq!(
            format!("{}", Error::UmountOldRoot(io::Error::from_raw_os_error(42))),
            "Failed to unmount the old jail root: No message of desired type (os error 42)",
        );
        assert_eq!(
            format!("{}", Error::UnexpectedListenerFd(42)),
            "Unexpected value for the socket listener fd: 42",
        );
        assert_eq!(
            format!("{}", Error::UnshareNewNs(io::Error::from_raw_os_error(42))),
            "Failed to unshare into new mount namespace: No message of desired type (os error 42)",
        );
        assert_eq!(
            format!("{}", Error::UnsetCloexec(io::Error::from_raw_os_error(42))),
            "Failed to unset the O_CLOEXEC flag on the socket fd: No message of desired type (os \
             error 42)",
        );
        assert_eq!(
            format!(
                "{}",
                Error::Write(file_path, io::Error::from_raw_os_error(2))
            ),
            format!("Failed to write to /foo/bar: {}", err2_str),
        );
    }

    #[test]
    fn test_to_cstring() {
        let path = Path::new("some_path");
        let cstring_path = to_cstring(path).unwrap();
        assert_eq!(cstring_path, CString::new("some_path").unwrap());
        let path_with_nul = Path::new("some_path\0");
        assert_eq!(
            format!("{}", to_cstring(path_with_nul).unwrap_err()),
            "Encountered interior \\0 while parsing a string"
        );
    }
}
