# Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Classes for working with microVMs.

This module defines `Microvm`, which can be used to create, test drive, and
destroy microvms.

# TODO

- Use the Firecracker Open API spec to populate Microvm API resource URLs.
"""

import json
import logging
import os
import re
import select
import shutil
import time
import weakref
from pathlib import Path

from threading import Lock
from retry import retry

import host_tools.logging as log_tools
import host_tools.cpu_load as cpu_tools
import host_tools.memory as mem_tools
import host_tools.network as net_tools

from framework import utils
from framework.defs import (
    MICROVM_KERNEL_RELPATH,
    MICROVM_FSFILES_RELPATH,
    FC_PID_FILE_NAME,
)
from framework.http import Session
from framework.jailer import JailerContext
from framework.resources import (
    Actions,
    Balloon,
    BootSource,
    Drive,
    DescribeInstance,
    FullConfig,
    InstanceVersion,
    Logger,
    MMDS,
    MachineConfigure,
    Metrics,
    Network,
    Vm,
    Vsock,
    SnapshotHelper,
)

LOG = logging.getLogger("microvm")
data_lock = Lock()


# pylint: disable=R0904
class Microvm:
    """Class to represent a Firecracker microvm.

    A microvm is described by a unique identifier, a path to all the resources
    it needs in order to be able to start and the binaries used to spawn it.
    Besides keeping track of microvm resources and exposing microvm API
    methods, `spawn()` and `kill()` can be used to start/end the microvm
    process.
    """

    SCREEN_LOGFILE = "/tmp/screen-{}.log"
    __log_data = ""

    def __init__(
        self,
        resource_path,
        fc_binary_path,
        jailer_binary_path,
        microvm_id,
        monitor_memory=True,
        bin_cloner_path=None,
    ):
        """Set up microVM attributes, paths, and data structures."""
        # Unique identifier for this machine.
        self._microvm_id = microvm_id

        # Compose the paths to the resources specific to this microvm.
        self._path = os.path.join(resource_path, microvm_id)
        self._kernel_path = os.path.join(self._path, MICROVM_KERNEL_RELPATH)
        self._fsfiles_path = os.path.join(self._path, MICROVM_FSFILES_RELPATH)
        self._kernel_file = ""
        self._rootfs_file = ""
        self._initrd_file = ""

        # The binaries this microvm will use to start.
        self._fc_binary_path = fc_binary_path
        assert os.path.exists(self._fc_binary_path)
        self._jailer_binary_path = jailer_binary_path
        assert os.path.exists(self._jailer_binary_path)

        # Create the jailer context associated with this microvm.
        self._jailer = JailerContext(
            jailer_id=self._microvm_id,
            exec_file=self._fc_binary_path,
        )
        self.jailer_clone_pid = None
        self._screen_log = None

        # Copy the /etc/localtime file in the jailer root
        self.jailer.copy_into_root("/etc/localtime", create_jail=True)

        # Now deal with the things specific to the api session used to
        # communicate with this machine.
        self._api_session = None
        self._api_socket = None

        # Session name is composed of the last part of the temporary path
        # allocated by the current test session and the unique id of this
        # microVM. It should be unique.
        self._session_name = (
            os.path.basename(os.path.normpath(resource_path)) + self._microvm_id
        )

        # nice-to-have: Put these in a dictionary.
        self.actions = None
        self.balloon = None
        self.boot = None
        self.desc_inst = None
        self.drive = None
        self.full_cfg = None
        self.logger = None
        self.metrics = None
        self.mmds = None
        self.network = None
        self.machine_cfg = None
        self.version = None
        self.vm = None
        self.vsock = None
        self.snapshot = None

        # Initialize the logging subsystem.
        self.logging_thread = None
        self._screen_pid = None

        # The ssh config dictionary is populated with information about how
        # to connect to a microVM that has ssh capability. The path of the
        # private key is populated by microvms with ssh capabilities and the
        # hostname is set from the MAC address used to configure the microVM.
        self._ssh_config = {
            "username": "root",
            "netns_file_path": self._jailer.netns_file_path(),
        }

        # Deal with memory monitoring.
        if monitor_memory:
            self._memory_monitor = mem_tools.MemoryMonitor()
        else:
            self._memory_monitor = None

        # Cpu load monitoring has to be explicitly enabled using
        # the `enable_cpu_load_monitor` method.
        self._cpu_load_monitor = None
        self._vcpus_count = None

        # External clone/exec tool, because Python can't into clone
        self.bin_cloner_path = bin_cloner_path

        # Flag checked in destructor to see abnormal signal-induced crashes.
        self.expect_kill_by_signal = False

        # MMDS content from file
        self._metadata_file = None

    def kill(self):
        """All clean up associated with this microVM should go here."""
        # pylint: disable=subprocess-run-check
        if self.logging_thread is not None:
            self.logging_thread.stop()

        if (
            self.expect_kill_by_signal is False
            and "Shutting down VM after intercepting signal" in self.log_data
        ):
            # Too late to assert at this point, pytest will still report the
            # test as passed. BUT we can dump full logs for debugging,
            # as well as an intentional eye-sore in the test report.
            LOG.error(self.log_data)

        if self._jailer.daemonize:
            if self.jailer_clone_pid:
                utils.run_cmd(
                    "kill -9 {}".format(self.jailer_clone_pid), ignore_return_code=True
                )
        else:
            # Killing screen will send SIGHUP to underlying Firecracker.
            # Needed to avoid false positives in case kill() is called again.
            self.expect_kill_by_signal = True
            utils.run_cmd("kill -9 {} || true".format(self.screen_pid))

        # Check if Firecracker was launched by the jailer in a new pid ns.
        fc_pid_in_new_ns = self.pid_in_new_ns

        if fc_pid_in_new_ns:
            # We need to explicitly kill the Firecracker pid, since it's
            # different from the jailer pid that was previously killed.
            utils.run_cmd(f"kill -9 {fc_pid_in_new_ns}", ignore_return_code=True)

        if self._memory_monitor and self._memory_monitor.is_alive():
            self._memory_monitor.signal_stop()
            self._memory_monitor.join(timeout=1)
            self._memory_monitor.check_samples()

        if self._cpu_load_monitor:
            self._cpu_load_monitor.signal_stop()
            self._cpu_load_monitor.join()
            self._cpu_load_monitor.check_samples()

    @property
    def firecracker_version(self):
        """Return the version of the Firecracker executable."""
        return self.version.get()

    @property
    def api_session(self):
        """Return the api session associated with this microVM."""
        return self._api_session

    @property
    def api_socket(self):
        """Return the socket used by this api session."""
        # TODO: this methods is only used as a workaround for getting
        # firecracker PID. We should not be forced to make this public.
        return self._api_socket

    @property
    def path(self):
        """Return the path on disk used that represents this microVM."""
        return self._path

    @property
    def id(self):
        """Return the unique identifier of this microVM."""
        return self._microvm_id

    @property
    def jailer(self):
        """Return the jailer context associated with this microVM."""
        return self._jailer

    @jailer.setter
    def jailer(self, jailer):
        """Setter for associating a different jailer to the default one."""
        self._jailer = jailer

    @property
    def kernel_file(self):
        """Return the name of the kernel file used by this microVM to boot."""
        return self._kernel_file

    @kernel_file.setter
    def kernel_file(self, path):
        """Set the path to the kernel file."""
        self._kernel_file = path

    @property
    def initrd_file(self):
        """Return the name of the initrd file used by this microVM to boot."""
        return self._initrd_file

    @initrd_file.setter
    def initrd_file(self, path):
        """Set the path to the initrd file."""
        self._initrd_file = path

    @property
    def log_data(self):
        """Return the log data.

        !!!!OBS!!!!: Do not use this to check for message existence and
        rather use self.check_log_message or self.find_log_message.
        """
        with data_lock:
            log_data = self.__log_data
        return log_data

    @property
    def rootfs_file(self):
        """Return the path to the image this microVM can boot into."""
        return self._rootfs_file

    @rootfs_file.setter
    def rootfs_file(self, path):
        """Set the path to the image associated."""
        self._rootfs_file = path

    @property
    def fsfiles(self):
        """Path to filesystem used by this microvm to attach new drives."""
        return self._fsfiles_path

    @property
    def ssh_config(self):
        """Get the ssh configuration used to ssh into some microVMs."""
        return self._ssh_config

    @ssh_config.setter
    def ssh_config(self, key, value):
        """Set the dict values inside this configuration."""
        setattr(self._ssh_config, key, value)

    @property
    def metadata_file(self):
        """Return the path to a file used for populating MMDS."""
        return self._metadata_file

    @metadata_file.setter
    def metadata_file(self, path):
        """Set the path to a file to use for populating MMDS."""
        self._metadata_file = path

    @property
    def memory_monitor(self):
        """Get the memory monitor."""
        return self._memory_monitor

    @property
    def state(self):
        """Get the InstanceInfo property and return the state field."""
        return json.loads(self.desc_inst.get().content)["state"]

    @property
    def started(self):
        """Get the InstanceInfo property and return the started field.

        This is kept for legacy snapshot support.
        """
        return json.loads(self.desc_inst.get().content)["started"]

    @memory_monitor.setter
    def memory_monitor(self, monitor):
        """Set the memory monitor."""
        self._memory_monitor = monitor

    @property
    def pid_in_new_ns(self):
        """Get the pid of the Firecracker process in the new namespace.

        Returns None if Firecracker was not launched in a new pid ns.
        """
        fc_pid = None

        pid_file_path = f"{self.jailer.chroot_path()}/{FC_PID_FILE_NAME}"
        if os.path.exists(pid_file_path):
            # Read the PID stored inside the file.
            with open(pid_file_path, encoding="utf-8") as file:
                fc_pid = int(file.readline())

        return fc_pid

    def flush_metrics(self, metrics_fifo):
        """Flush the microvm metrics.

        Requires specifying the configured metrics file.
        """
        # Empty the metrics pipe.
        _ = metrics_fifo.sequential_reader(100)

        response = self.actions.put(action_type="FlushMetrics")
        assert self.api_session.is_status_no_content(response.status_code)

        lines = metrics_fifo.sequential_reader(100)
        assert len(lines) == 1

        return json.loads(lines[0])

    def get_all_metrics(self, metrics_fifo):
        """Return all metric data points written by FC.

        Requires specifying the configured metrics file.
        """
        # Empty the metrics pipe.
        response = self.actions.put(action_type="FlushMetrics")
        assert self.api_session.is_status_no_content(response.status_code)

        return metrics_fifo.sequential_reader(1000)

    def append_to_log_data(self, data):
        """Append a message to the log data."""
        with data_lock:
            self.__log_data += data

    def enable_cpu_load_monitor(self, threshold):
        """Enable the cpu load monitor."""
        process_pid = self.jailer_clone_pid
        # We want to monitor the emulation thread, which is currently
        # the first one created.
        # A possible improvement is to find it by name.
        thread_pid = self.jailer_clone_pid
        self._cpu_load_monitor = cpu_tools.CpuLoadMonitor(
            process_pid, thread_pid, threshold
        )
        self._cpu_load_monitor.start()

    def copy_to_jail_ramfs(self, src):
        """Copy a file to a jail ramfs."""
        filename = os.path.basename(src)
        dest_path = os.path.join(self.jailer.chroot_ramfs_path(), filename)
        jailed_path = os.path.join("/", self.jailer.ramfs_subdir_name, filename)
        shutil.copy(src, dest_path)
        cmd = "chown {}:{} {}".format(self.jailer.uid, self.jailer.gid, dest_path)
        utils.run_cmd(cmd)
        return jailed_path

    def create_jailed_resource(self, path, create_jail=False):
        """Create a hard link to some resource inside this microvm."""
        return self.jailer.jailed_path(path, create=True, create_jail=create_jail)

    def get_jailed_resource(self, path):
        """Get the relative jailed path to a resource."""
        return self.jailer.jailed_path(path, create=False)

    def chroot(self):
        """Get the chroot of this microVM."""
        return self.jailer.chroot_path()

    def setup(self):
        """Create a microvm associated folder on the host.

        The root path of some microvm is `self._path`.
        Also creates the where essential resources (i.e. kernel and root
        filesystem) will reside.

         # Microvm Folder Layout

             There is a fixed tree layout for a microvm related folder:

             ``` file_tree
             <microvm_uuid>/
                 kernel/
                     <kernel_file_n>
                     ....
                 fsfiles/
                     <fsfile_n>
                     <initrd_file_n>
                     <ssh_key_n>
                     <other fsfiles>
                     ...
                  ...
             ```
        """
        os.makedirs(self._path, exist_ok=True)
        os.makedirs(self._kernel_path, exist_ok=True)
        os.makedirs(self._fsfiles_path, exist_ok=True)

    @property
    def screen_log(self):
        """Get the screen log file."""
        return self._screen_log

    @property
    def screen_pid(self):
        """Get the screen PID."""
        return self._screen_pid

    @property
    def vcpus_count(self):
        """Get the vcpus count."""
        return self._vcpus_count

    @vcpus_count.setter
    def vcpus_count(self, vcpus_count: int):
        """Set the vcpus count."""
        self._vcpus_count = vcpus_count

    def pin_vmm(self, cpu_id: int) -> bool:
        """Pin the firecracker process VMM thread to a cpu list."""
        if self.jailer_clone_pid:
            for thread in utils.ProcessManager.get_threads(self.jailer_clone_pid)[
                "firecracker"
            ]:
                utils.ProcessManager.set_cpu_affinity(thread, [cpu_id])
                return True
        return False

    def pin_vcpu(self, vcpu_id: int, cpu_id: int):
        """Pin the firecracker vcpu thread to a cpu list."""
        if self.jailer_clone_pid:
            for thread in utils.ProcessManager.get_threads(self.jailer_clone_pid)[
                f"fc_vcpu {vcpu_id}"
            ]:
                utils.ProcessManager.set_cpu_affinity(thread, [cpu_id])
            return True
        return False

    def pin_api(self, cpu_id: int):
        """Pin the firecracker process API server thread to a cpu list."""
        if self.jailer_clone_pid:
            for thread in utils.ProcessManager.get_threads(self.jailer_clone_pid)[
                "fc_api"
            ]:
                utils.ProcessManager.set_cpu_affinity(thread, [cpu_id])
            return True
        return False

    def spawn(
        self,
        create_logger=True,
        log_file="log_fifo",
        log_level="Info",
        use_ramdisk=False,
        create_netns=True,
        metrics_path=None,
    ):
        """Start a microVM as a daemon or in a screen session."""
        # pylint: disable=subprocess-run-check
        self._jailer.setup(create_netns, use_ramdisk=use_ramdisk)
        self._api_socket = self._jailer.api_socket_path()
        self._api_session = Session()

        self.actions = Actions(self._api_socket, self._api_session)
        self.balloon = Balloon(self._api_socket, self._api_session)
        self.boot = BootSource(self._api_socket, self._api_session)
        self.desc_inst = DescribeInstance(self._api_socket, self._api_session)
        self.full_cfg = FullConfig(self._api_socket, self._api_session)
        self.logger = Logger(self._api_socket, self._api_session)
        self.version = InstanceVersion(
            self._api_socket, self._fc_binary_path, self._api_session
        )
        self.machine_cfg = MachineConfigure(
            self._api_socket, self._api_session, self.firecracker_version
        )
        self.metrics = Metrics(self._api_socket, self._api_session)
        self.mmds = MMDS(self._api_socket, self._api_session)
        self.network = Network(self._api_socket, self._api_session)
        self.snapshot = SnapshotHelper(self._api_socket, self._api_session)
        self.drive = Drive(self._api_socket, self._api_session)
        self.vm = Vm(self._api_socket, self._api_session)
        self.vsock = Vsock(self._api_socket, self._api_session)

        if create_logger:
            log_fifo_path = os.path.join(self.path, log_file)
            log_fifo = log_tools.Fifo(log_fifo_path)
            self.create_jailed_resource(log_fifo.path, create_jail=True)
            # The default value for `level`, when configuring the
            # logger via cmd line, is `Warning`. We set the level
            # to `Info` to also have the boot time printed in fifo.
            self.jailer.extra_args.update({"log-path": log_file, "level": log_level})
            self.start_console_logger(log_fifo)

        if metrics_path is not None:
            self.create_jailed_resource(metrics_path, create_jail=True)
            metrics_path = Path(metrics_path)
            self.jailer.extra_args.update({"metrics-path": metrics_path.name})

        if self.metadata_file:
            if os.path.exists(self.metadata_file):
                LOG.debug("metadata file exists, adding as a jailed resource")
                self.create_jailed_resource(self.metadata_file, create_jail=True)
            self.jailer.extra_args.update(
                {"metadata": os.path.basename(self.metadata_file)}
            )

        jailer_param_list = self._jailer.construct_param_list()

        # When the daemonize flag is on, we want to clone-exec into the
        # jailer rather than executing it via spawning a shell. Going
        # forward, we'll probably switch to this method for running
        # Firecracker in general, because it represents the way it's meant
        # to be run by customers (together with CLONE_NEWPID flag).
        #
        # We have to use an external tool for CLONE_NEWPID, because
        # 1) Python doesn't provide os.clone() interface, and
        # 2) Python's ctypes libc interface appears to be broken, causing
        # our clone / exec to deadlock at some point.
        if self._jailer.daemonize:
            self.daemonize_jailer(jailer_param_list)
        else:
            # This file will collect any output from 'screen'ed Firecracker.
            self._screen_log = self.SCREEN_LOGFILE.format(self._session_name)
            screen_pid, binary_pid = utils.start_screen_process(
                self._screen_log,
                self._session_name,
                self._jailer_binary_path,
                jailer_param_list,
            )
            self._screen_pid = screen_pid
            self.jailer_clone_pid = binary_pid

        # Wait for the jailer to create resources needed, and Firecracker to
        # create its API socket.
        # We expect the jailer to start within 80 ms. However, we wait for
        # 1 sec since we are rechecking the existence of the socket 5 times
        # and leave 0.2 delay between them.
        if "no-api" not in self._jailer.extra_args:
            self._wait_create()
        if create_logger:
            self.check_log_message("Running Firecracker")

    @retry(delay=0.2, tries=5)
    def _wait_create(self):
        """Wait until the API socket and chroot folder are available."""
        os.stat(self._jailer.api_socket_path())

    @retry(delay=0.1, tries=5)
    def check_log_message(self, message):
        """Wait until `message` appears in logging output."""
        assert message in self.log_data

    @retry(delay=0.1, tries=5)
    def check_any_log_message(self, messages):
        """Wait until any message in `messages` appears in logging output."""
        for message in messages:
            if message in self.log_data:
                return
        raise AssertionError(
            f"`{messages}` were not found in this log: {self.log_data}"
        )

    @retry(delay=0.1, tries=5)
    def find_log_message(self, regex):
        """Wait until `regex` appears in logging output and return it."""
        reg_res = re.findall(regex, self.log_data)
        assert reg_res
        return reg_res

    def serial_input(self, input_string):
        """Send a string to the Firecracker serial console via screen."""
        input_cmd = 'screen -S {session} -p 0 -X stuff "{input_string}"'
        utils.run_cmd(
            input_cmd.format(session=self._session_name, input_string=input_string)
        )

    def basic_config(
        self,
        vcpu_count: int = 2,
        smt: bool = None,
        mem_size_mib: int = 256,
        add_root_device: bool = True,
        boot_args: str = None,
        use_initrd: bool = False,
        track_dirty_pages: bool = False,
        rootfs_io_engine=None,
    ):
        """Shortcut for quickly configuring a microVM.

        It handles:
        - CPU and memory.
        - Kernel image (will load the one in the microVM allocated path).
        - Root File System (will use the one in the microVM allocated path).
        - Does not start the microvm.

        The function checks the response status code and asserts that
        the response is within the interval [200, 300).
        """
        response = self.machine_cfg.put(
            vcpu_count=vcpu_count,
            smt=smt,
            mem_size_mib=mem_size_mib,
            track_dirty_pages=track_dirty_pages,
        )
        assert self._api_session.is_status_no_content(
            response.status_code
        ), response.text

        if self.memory_monitor:
            self.memory_monitor.guest_mem_mib = mem_size_mib
            self.memory_monitor.pid = self.jailer_clone_pid
            self.memory_monitor.start()

        boot_source_args = {
            "kernel_image_path": self.create_jailed_resource(self.kernel_file),
            "boot_args": boot_args,
        }

        if use_initrd and self.initrd_file != "":
            boot_source_args.update(
                initrd_path=self.create_jailed_resource(self.initrd_file)
            )

        response = self.boot.put(**boot_source_args)
        assert self._api_session.is_status_no_content(
            response.status_code
        ), response.text

        if add_root_device and self.rootfs_file != "":
            # Add the root file system with rw permissions.
            response = self.drive.put(
                drive_id="rootfs",
                path_on_host=self.create_jailed_resource(self.rootfs_file),
                is_root_device=True,
                is_read_only=False,
                io_engine=rootfs_io_engine,
            )
            assert self._api_session.is_status_no_content(
                response.status_code
            ), response.text

    def daemonize_jailer(self, jailer_param_list):
        """Daemonize the jailer."""
        if self.bin_cloner_path and self.jailer.new_pid_ns is not True:
            cmd = (
                [self.bin_cloner_path] + [self._jailer_binary_path] + jailer_param_list
            )
            _p = utils.run_cmd(cmd)
            # Terrible hack to make the tests fail when starting the
            # jailer fails with a panic. This is needed because we can't
            # get the exit code of the jailer. In newpid_clone.c we are
            # not waiting for the process and we always return 0 if the
            # clone was successful (which in most cases will be) and we
            # don't do anything if the jailer was not started
            # successfully.
            if _p.stderr.strip():
                raise Exception(_p.stderr)
            self.jailer_clone_pid = int(_p.stdout.rstrip())
        else:
            # Fallback mechanism for when we offload PID namespacing
            # to the jailer.
            _pid = os.fork()
            if _pid == 0:
                os.execv(
                    self._jailer_binary_path,
                    [self._jailer_binary_path] + jailer_param_list,
                )
            self.jailer_clone_pid = _pid

    def add_drive(
        self,
        drive_id,
        file_path,
        root_device=False,
        is_read_only=False,
        partuuid=None,
        cache_type=None,
        io_engine=None,
        use_ramdisk=False,
    ):
        """Add a block device."""
        response = self.drive.put(
            drive_id=drive_id,
            path_on_host=(
                self.copy_to_jail_ramfs(file_path)
                if use_ramdisk
                else self.create_jailed_resource(file_path)
            ),
            is_root_device=root_device,
            is_read_only=is_read_only,
            partuuid=partuuid,
            cache_type=cache_type,
            io_engine=io_engine,
        )
        assert self.api_session.is_status_no_content(response.status_code)

    def patch_drive(self, drive_id, file):
        """Modify/patch an existing block device."""
        response = self.drive.patch(
            drive_id=drive_id,
            path_on_host=self.create_jailed_resource(file.path),
        )
        assert self.api_session.is_status_no_content(response.status_code)

    def put_network(
            self, iface_id, tapname, guest_mac,
            allow_mmds_requests=False,
            tx_rate_limiter=None,
            rx_rate_limiter=None
    ):
        """Attach a network device."""
        response = self.network.put(
            iface_id=iface_id,
            host_dev_name=tapname,
            guest_mac=guest_mac,
            allow_mmds_requests=allow_mmds_requests,
            tx_rate_limiter=tx_rate_limiter,
            rx_rate_limiter=rx_rate_limiter
        )
        assert self._api_session.is_status_no_content(response.status_code)

    def ssh_network_config(
        self,
        network_config,
        iface_id,
        allow_mmds_requests=False,
        tx_rate_limiter=None,
        rx_rate_limiter=None,
        tapname=None,
    ):
        """Create a host tap device and a guest network interface.

        'network_config' is used to generate 2 IPs: one for the tap device
        and one for the microvm. Adds the hostname of the microvm to the
        ssh_config dictionary.
        :param network_config: UniqueIPv4Generator instance
        :param iface_id: the interface id for the API request
        the guest on this interface towards the MMDS address are
        intercepted and processed by the device model.
        :param tx_rate_limiter: limit the tx rate
        :param rx_rate_limiter: limit the rx rate
        :return: an instance of the tap which needs to be kept around until
        cleanup is desired, the configured guest and host ips, respectively.
        """
        # Create tap before configuring interface.
        tapname = tapname or (self.id[:8] + "tap" + iface_id)
        (host_ip, guest_ip) = network_config.get_next_available_ips(2)
        tap = self.create_tap_and_ssh_config(
            host_ip, guest_ip, network_config.get_netmask_len(), tapname
        )
        guest_mac = net_tools.mac_from_ip(guest_ip)

        self.put_network(iface_id, tapname, guest_mac, allow_mmds_requests,
                         tx_rate_limiter, rx_rate_limiter)

        return tap, host_ip, guest_ip

    def create_tap_and_ssh_config(self, host_ip, guest_ip, netmask_len, tapname=None):
        """Create tap device and configure ssh."""
        assert tapname is not None
        tap = net_tools.Tap(
            tapname, self._jailer.netns, ip="{}/{}".format(host_ip, netmask_len)
        )
        self.config_ssh(guest_ip)
        return tap

    def config_ssh(self, guest_ip):
        """Configure ssh."""
        self.ssh_config["hostname"] = guest_ip

    def start(self, check=True):
        """Start the microvm.

        This function has asserts to validate that the microvm boot success.
        """
        # Check that the VM has not started yet
        try:
            assert self.state == "Not started"
        except KeyError:
            assert self.started is False

        response = self.actions.put(action_type="InstanceStart")

        if check:
            assert self._api_session.is_status_no_content(
                response.status_code
            ), response.text

            # Check that the VM has started
            try:
                assert self.state == "Running"
            except KeyError:
                assert self.started is True

    def pause_to_snapshot(
        self, mem_file_path=None, snapshot_path=None, diff=False, version=None
    ):
        """Pauses the microVM, and creates snapshot.

        This function validates that the microVM pauses successfully and
        creates a snapshot.
        """
        assert mem_file_path is not None, "Please specify mem_file_path."
        assert snapshot_path is not None, "Please specify snapshot_path."

        response = self.vm.patch(state="Paused")
        assert self.api_session.is_status_no_content(response.status_code)

        self.api_session.untime()
        response = self.snapshot.create(
            mem_file_path=mem_file_path,
            snapshot_path=snapshot_path,
            diff=diff,
            version=version,
        )
        assert self.api_session.is_status_no_content(
            response.status_code
        ), response.text

    def restore_from_snapshot(
        self,
        *,
        snapshot_mem: Path,
        snapshot_vmstate: Path,
        snapshot_disks: list[Path],
        snapshot_is_diff: bool = False,
    ):
        """
        Restores a snapshot, and resumes the microvm
        """

        # Hardlink all the snapshot files into the microvm jail.
        jailed_mem = self.create_jailed_resource(snapshot_mem)
        jailed_vmstate = self.create_jailed_resource(snapshot_vmstate)

        assert len(snapshot_disks) > 0, "Snapshot requires at least one disk."
        jailed_disks = []
        for disk in snapshot_disks:
            jailed_disks.append(self.create_jailed_resource(disk))

        response = self.snapshot.load(
            mem_file_path=jailed_mem,
            snapshot_path=jailed_vmstate,
            diff=snapshot_is_diff,
            resume=True,
        )
        assert response.ok
        return True

    def start_console_logger(self, log_fifo):
        """
        Start a thread that monitors the microVM console.

        The console output will be redirected to the log file.
        """

        def monitor_fd(microvm, path):
            try:
                fd = open(path, "r", encoding="utf-8")
                while True:
                    try:
                        if microvm().logging_thread.stopped():
                            return
                        data = fd.readline()
                        if data:
                            microvm().append_to_log_data(data)
                    except AttributeError as _:
                        # This means that the microvm object was destroyed and
                        # we are using a None reference.
                        return
            except IOError as error:
                # pylint: disable=W0150
                try:
                    LOG.error(
                        "[%s] IOError while monitoring fd:" " %s", microvm().id, error
                    )
                    microvm().append_to_log_data(str(error))
                except AttributeError as _:
                    # This means that the microvm object was destroyed and
                    # we are using a None reference.
                    pass
                finally:
                    return

        self.logging_thread = utils.StoppableThread(
            target=monitor_fd, args=(weakref.ref(self), log_fifo.path), daemon=True
        )
        self.logging_thread.start()

    def __del__(self):
        """Teardown the object."""
        self.kill()


class Serial:
    """Class for serial console communication with a Microvm."""

    RX_TIMEOUT_S = 20

    def __init__(self, vm):
        """Initialize a new Serial object."""
        self._poller = None
        self._vm = vm

    def open(self):
        """Open a serial connection."""
        # Open the screen log file.
        if self._poller is not None:
            # serial already opened
            return

        screen_log_fd = os.open(self._vm.screen_log, os.O_RDONLY)
        self._poller = select.poll()
        self._poller.register(screen_log_fd, select.POLLIN | select.POLLHUP)

    def tx(self, input_string, end="\n"):
        # pylint: disable=invalid-name
        # No need to have a snake_case naming style for a single word.
        r"""Send a string terminated by an end token (defaulting to "\n")."""
        self._vm.serial_input(input_string + end)

    def rx_char(self):
        """Read a single character."""
        result = self._poller.poll(0.1)

        for fd, flag in result:
            if flag & select.POLLHUP:
                assert False, "Oh! The console vanished before test completed."

            if flag & select.POLLIN:
                output_char = str(os.read(fd, 1), encoding="utf-8", errors="ignore")
                return output_char

        return ""

    def rx(self, token="\n"):
        # pylint: disable=invalid-name
        # No need to have a snake_case naming style for a single word.
        r"""Read a string delimited by an end token (defaults to "\n")."""
        rx_str = ""
        start = time.time()
        while True:
            rx_str += self.rx_char()
            if rx_str.endswith(token):
                break
            if (time.time() - start) >= self.RX_TIMEOUT_S:
                self._vm.kill()
                assert False

        return rx_str
