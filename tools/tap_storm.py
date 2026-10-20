import os, time, sys
PIPE = sys.argv[1]
N = int(sys.argv[2])
def frame(kind, x, y):
    return bytes((0xAA, kind, x & 0xFF, x >> 8, y & 0xFF, y >> 8))
for i in range(N):
    infd = os.open(PIPE + ".in", os.O_WRONLY)
    os.write(infd, frame(0x01, 465, 925))
    time.sleep(0.06)
    os.write(infd, frame(0x00, 0, 0))
    os.close(infd)
    time.sleep(1.8)
    print(f"tap {i+1}/{N}", flush=True)
