import hashlib
import os
import os.path
import re
from select import select
from socket import socket, AF_UNIX, SOCK_STREAM
from threading import Thread, Event

from host_tools.network import SSHConnection


ECHO_SERVER_PORT = 5252
CONNECTION_COUNT = 1
BLOB_SIZE = 100 * 1024 * 1024


class HostEchoServer(Thread):

    def __init__(self, vm, path):
        super().__init__()
        self.vm = vm
        self.sock = socket(AF_UNIX, SOCK_STREAM)
        self.sock.bind(path)
        self.sock.listen(10)
        self.error = None
        self.clients = []
        self.exit_evt = Event()

        vm.create_jailed_resource(path)

    def run(self):
        try:
            self._run()
        except Exception as e:
            self.error = e

    def _run(self):
        while not self.exit_evt.is_set():
            watch_list = self.clients + [self.sock]
            rd_list, _, _ = select(watch_list, [], [], 1)
            for rdo in rd_list:
                if rdo == self.sock:
                    (client, _) = self.sock.accept()
                    self.clients.append(client)
                    continue
                buf = rdo.recv(64 * 1024)
                if len(buf) == 0:
                    self.clients.remove(rdo)
                    continue
                sent = 0
                while sent < len(buf):
                    sent += rdo.send(buf[sent:])

    def exit(self):
        self.exit_evt.set()
        self.join()


class GuestPongWorker(Thread):

    def __init__(self, id, conn, blob_path):
        super().__init__()
        self.id = id
        self.conn = conn
        self.blob_path = blob_path
        self.hash = None

    def run(self):
        cmd = "cat {} | vsock_helper pong 2 {} | md5sum | cut -f1 -d\\ ".format(
            self.blob_path, ECHO_SERVER_PORT
        )
        print("pong {} starting".format(self.id))
        exit_code, stdout, _ = self.conn.execute_command(cmd)
        if exit_code == 0:
            self.hash = stdout.read().decode("utf8").strip()
        print("pong {} done: {}, {}".format(self.id, exit_code, self.hash))


class HostEchoWorker(Thread):

    def __init__(self, id, uds_path, blob_path):
        super().__init__()
        self.id = id
        self.uds_path = uds_path
        self.blob_path = blob_path
        self.hash = None
        self.error = None

        self.nsent = 0
        self.nrecvd = 0

    def run(self):
        try:
            self._run()
        except Exception as e:
            self.error = e

    def _run(self):
        sock = socket(AF_UNIX, SOCK_STREAM)
        sock.connect(self.uds_path)
        buf = bytearray("CONNECT {}\n".format(ECHO_SERVER_PORT).encode("utf-8"))
        sock.send(buf)
        buf = sock.recv(64).decode("utf-8")
        assert re.match("OK [0-9]+$", buf) is not None

        blob_file = open(self.blob_path, 'rb')
        hash_obj = hashlib.md5()

        while True:
            _last_nrecvd = self.nrecvd
            _last_nsent = self.nsent

            buf = blob_file.read(64*1024)
            if len(buf) == 0:
                break
            offset = 0
            while offset < len(buf):
                sent = sock.send(buf[offset:])
                #print("SENT: {}".format(sent))
                offset += sent
                self.nsent += sent

            rbuf = b""
            while len(rbuf) < len(buf):
                rbuf += sock.recv(len(buf) - len(rbuf))
            self.nrecvd += len(rbuf)
            hash_obj.update(rbuf)


            def bdump(buf):
                return " ".join(["{:02x}".format(b) for b in buf])
            if buf != rbuf:
                bd1 = bdump(buf[:32])
                bd2 = bdump(rbuf[:32])
                # print("Worker {} ERROR:\n {}\n {}".format(self.id, bd1, bd2))
                print("Worker {} ERROR at S={:08x}, R={:08x}".format(
                    self.id, _last_nsent, _last_nrecvd
                ))
                print("  {}".format(bd1))
                print("  {}".format(bd2))
                return

        # while True:
        #     rdl = select([sock], [], [], 0)
        #     if sock not in rdl:
        #         break
        #     buf = sock.recv(4096)
        #     self.nrecvd += len(buf)
        #     hash_obj.update(buf)

        self.hash = hash_obj.hexdigest()


def test_vsock(
        test_microvm_with_ssh,
        network_config,
        aux_bin_paths,
        test_session_root_path
):
    """Vsock
    """
    vm = test_microvm_with_ssh
    vm.spawn()

    vm.basic_config()
    _tap, _, _ = vm.ssh_network_config(network_config, '1')
    vm.vsock.put(
        vsock_id="vsock0",
        guest_cid=3,
        uds_path="/vsock.sock"
    )

    vm.start()

    blob_path, blob_hash = _make_blob(test_session_root_path)
    vm_blob_path = "/tmp/vsock-test.blob"

    conn = SSHConnection(vm.ssh_config)
    conn.scp_file(aux_bin_paths['vsock_helper'], '/bin/vsock_helper')
    conn.scp_file(blob_path, vm_blob_path)

    # path = os.path.join(vm.path, "vsock.sock_{}".format(ECHO_SERVER_PORT))
    # _test_egress(vm, path, vm_blob_path, blob_hash)

    path = os.path.join(vm.jailer.chroot_path(), "vsock.sock")
    _test_ingress(vm, path, blob_path, blob_hash)


def _make_blob(test_session_root_path):

    blob_path = os.path.join(test_session_root_path, "vsock-test.blob")
    blob_file = open(blob_path, 'wb')
    left = BLOB_SIZE
    blob_hash = hashlib.md5()
    while left > 0:
        count = min(left, 4096)
        buf = os.urandom(count)
        blob_hash.update(buf)
        blob_file.write(buf)
        left -= count
    blob_file.close()

    return blob_path, blob_hash.hexdigest()


def _test_egress(vm, server_path, blob_path, blob_hash):

    echo_server = HostEchoServer(vm, server_path)
    echo_server.start()

    workers = []
    for i in range(CONNECTION_COUNT):
        conn = SSHConnection(vm.ssh_config)
        worker = GuestPongWorker(i, conn, blob_path)
        workers.append(worker)
        worker.start()

    for w in workers:
        w.join()

    echo_server.exit()
    assert echo_server.error is None

    for w in workers:
        assert w.hash == blob_hash


def _test_ingress(vm, uds_path, blob_path, blob_hash):

    conn = SSHConnection(vm.ssh_config)
    cmd = "vsock_helper echosrv -d {}". format(ECHO_SERVER_PORT)
    ecode, _, _ = conn.execute_command(cmd)
    assert ecode == 0

    workers = []
    for i in range(CONNECTION_COUNT):
        worker = HostEchoWorker(i, uds_path, blob_path)
        workers.append(worker)
        worker.start()

    for w in workers:
        w.join()

    for w in workers:
        print("worker {}: S={}, R={}".format(w.id, w.nsent, w.nrecvd))
        assert w.hash == blob_hash
