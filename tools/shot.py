import socket, sys, time
def send(cmd, sockpath):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sockpath)
    s.sendall(cmd.encode() + b"\n")
    time.sleep(0.3)
    try:
        s.settimeout(1); s.recv(4096)
    except socket.timeout: pass
    s.close()
if __name__ == "__main__":
    send(sys.argv[1], sys.argv[2])
