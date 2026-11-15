#!/usr/bin/env python3
"""KunxOS QEMU 触摸注入工具（RISC-V 16550 UART 协议）。

与内核 kunxcore/src/input.rs 的 UART 协议帧解析器配合，把触摸手势编码为
6 字节小端协议帧，经 QEMU 的串口 TCP 服务器（-serial tcp::1234,server）发送。

协议帧：
  0xAA 0x01 x_lo x_hi y_lo y_hi   触摸按下（坐标 x,y，屏幕像素坐标）
  0xAA 0x02 x_lo x_hi y_lo y_hi   触摸移动
  0xAA 0x00 0x00 0x00 0x00 0x00   触摸抬起

用法：
  python3 tools/input_inject.py tap 120 300 [--hold 100]
  python3 tools/input_inject.py swipe 100 500 300 500 [--steps 12 --step-ms 15]
  python3 tools/input_inject.py longpress 120 300 [--hold 900]
  python3 tools/input_inject.py console

配合 make run-input（QEMU 串口挂到 127.0.0.1:1234）使用。
"""

import argparse
import socket
import sys
import time


FRAME_SYNC = 0xAA
FRAME_UP = 0x00
FRAME_DOWN = 0x01
FRAME_MOVE = 0x02


def frame(kind, x=0, y=0):
    x &= 0xFFFF
    y &= 0xFFFF
    return bytes([FRAME_SYNC, kind, x & 0xFF, (x >> 8) & 0xFF, y & 0xFF, (y >> 8) & 0xFF])


def connect(host, port):
    return socket.create_connection((host, port), timeout=5)


def send_all(sock, data):
    sock.sendall(data)


def tap(sock, x, y, hold_ms):
    send_all(sock, frame(FRAME_DOWN, x, y))
    time.sleep(hold_ms / 1000.0)
    send_all(sock, frame(FRAME_UP))


def swipe(sock, x1, y1, x2, y2, steps, step_ms):
    send_all(sock, frame(FRAME_DOWN, x1, y1))
    time.sleep(step_ms / 1000.0)
    for i in range(1, steps + 1):
        t = i / float(steps)
        x = int(round(x1 + (x2 - x1) * t))
        y = int(round(y1 + (y2 - y1) * t))
        send_all(sock, frame(FRAME_MOVE, x, y))
        time.sleep(step_ms / 1000.0)
    send_all(sock, frame(FRAME_UP))


def longpress(sock, x, y, hold_ms):
    send_all(sock, frame(FRAME_DOWN, x, y))
    time.sleep(hold_ms / 1000.0)
    send_all(sock, frame(FRAME_UP))


def console(host, port):
    """连接串口并转发收到的字节到 stdout（用于观察内核日志）。"""
    s = connect(host, port)
    s.settimeout(1.0)
    try:
        while True:
            try:
                data = s.recv(4096)
                if not data:
                    break
                sys.stdout.buffer.write(data)
                sys.stdout.buffer.flush()
            except socket.timeout:
                continue
    except KeyboardInterrupt:
        pass
    finally:
        s.close()


def main():
    p = argparse.ArgumentParser(description="KunxOS QEMU touch injector (UART protocol)")
    p.add_argument("--host", default="127.0.0.1", help="QEMU serial TCP host (default 127.0.0.1)")
    p.add_argument("--port", type=int, default=1234, help="QEMU serial TCP port (default 1234)")
    sub = p.add_subparsers(dest="cmd", required=True)

    p_tap = sub.add_parser("tap", help="点击 (down -> up)")
    p_tap.add_argument("x", type=int)
    p_tap.add_argument("y", type=int)
    p_tap.add_argument("--hold", type=int, default=100, help="按住时长(ms)")

    p_swipe = sub.add_parser("swipe", help="滑动 (down -> move* -> up)")
    p_swipe.add_argument("x1", type=int)
    p_swipe.add_argument("y1", type=int)
    p_swipe.add_argument("x2", type=int)
    p_swipe.add_argument("y2", type=int)
    p_swipe.add_argument("--steps", type=int, default=12, help="中间移动点数")
    p_swipe.add_argument("--step-ms", type=int, default=15, help="每个移动点间隔(ms)")

    p_long = sub.add_parser("longpress", help="长按 (down -> 等待 >800ms -> up)")
    p_long.add_argument("x", type=int)
    p_long.add_argument("y", type=int)
    p_long.add_argument("--hold", type=int, default=900, help="按住时长(ms)")

    sub.add_parser("console", help="连接并转发内核串口日志到 stdout")

    args = p.parse_args()

    if args.cmd == "console":
        console(args.host, args.port)
        return

    s = connect(args.host, args.port)
    try:
        if args.cmd == "tap":
            tap(s, args.x, args.y, args.hold)
        elif args.cmd == "swipe":
            swipe(s, args.x1, args.y1, args.x2, args.y2, args.steps, args.step_ms)
        elif args.cmd == "longpress":
            longpress(s, args.x, args.y, args.hold)
        else:
            p.error("unknown command")
    finally:
        s.close()


if __name__ == "__main__":
    main()
