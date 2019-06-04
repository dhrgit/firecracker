#include <sys/socket.h>
#include <sys/types.h>
#include <sys/select.h>
#include <linux/vm_sockets.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <stdio.h>
#include <errno.h>


int print_usage() {
    fprintf(stderr, "Usage: ./vsock-helper {echosrv [-d] <port> | pong <cid> <port>}\n");
    return -1;
}

int xfer(int src_fd, int dst_fd) {
    char buf[4096];
    size_t offset = 0;
    size_t count =  read(src_fd, buf, sizeof(buf));

    if (!count) return 0;
    if (count < 0) return -1;

    do {
        size_t written = 0;
        written = write(dst_fd, &buf[offset], count - offset);
        if (written <= 0) return -1;
        offset += written;
    } while (offset < count);

    return offset;
}


int run_echosrv(uint32_t port) {

    struct sockaddr_vm vsock_addr = {
        .svm_family = AF_VSOCK,
        .svm_port = port,
        .svm_cid = VMADDR_CID_ANY
    };

    int srv_sock = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (srv_sock < 0) {
        perror("socket()");
        return -1;
    }

    int res = bind(srv_sock, (struct sockaddr*)&vsock_addr, sizeof(struct sockaddr_vm));
    if (res) {
        perror("bind()");
        return -1;
    }

    res = listen(srv_sock, 5);
    if (res) {
        perror("listen()");
        return -1;
    }

    for (;;) {
        struct sockaddr cl_addr;
        socklen_t sockaddr_len = sizeof(cl_addr);
        int cl_sock = accept(srv_sock, &cl_addr, &sockaddr_len);
        if (cl_sock < 0) {
            perror("accept()");
            continue;
        }

        int pid = fork();
        if (pid < 0) {
            perror("fork()");
            close(cl_sock);
            continue;
        }

        if (!pid) {
            int res;
            do {
                res = xfer(cl_sock, cl_sock);
            } while (res > 0);
            return res;
        }

        printf("New client forked...\n");
    }

	return 0;
}


int run_pong(uint32_t cid, uint32_t port) {

    int sock = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (sock < 0) {
        perror("socket()");
        return -1;
    }

    struct sockaddr_vm vsock_addr = {
        .svm_family = AF_VSOCK,
        .svm_port = port,
        .svm_cid = cid
    };
    if (connect(sock, (struct sockaddr*)&vsock_addr, sizeof(vsock_addr)) < 0) {
        perror("connect()");
        return -1;
    }

    int res;
    for (;;) {
        res = xfer(STDIN_FILENO, sock);
        if (!res) break;
        if (res < 0) return res;
        res = xfer(sock, STDOUT_FILENO);
        if (res <= 0) return -1;
    }

    return 0;
}


int main(int argc, char **argv) {

    if (argc < 3) {
        return print_usage();
    }

    if (strcmp(argv[1], "echosrv") == 0) {
        uint32_t port;
        if (strcmp(argv[2], "-d") == 0) {
            if (argc < 4) {
                return print_usage();
            }
            port = atoi(argv[3]);
            if (!port) {
                return print_usage();
            }
            int pid = fork();
            if (pid < 0) return -1;
            if (pid) {
                printf("Forked vsock echo daemon listening on port %d\n", port);
                return 0;
            }
            setsid();
            close(STDIN_FILENO);
            close(STDOUT_FILENO);
        }
        else {
            port = atoi(argv[2]);
            if (!port) {
                return print_usage();
            }
        }
        return run_echosrv(port);
    }

    if (strcmp(argv[1], "pong") == 0) {
        if (argc != 4) {
            return print_usage();
        }
        uint32_t cid = atoi(argv[2]);
        uint32_t port = atoi(argv[3]);
        if (!cid || !port) {
            return print_usage();
        }
        return run_pong(cid, port);
    }

    return print_usage();
}
