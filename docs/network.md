# KunxOS 网络功能演示指南

KunxOS 内核自带完整的最小网络栈（virtio-net + ARP/IP/ICMP/UDP/TCP/DNS），
经 QEMU slirp 用户网络可与宿主机真实互通。

## 启动

```sh
# 常用：带网络 + 端口转发（hostfwd）
qemu-system-aarch64 -M virt -cpu cortex-a72 -m 512M \
    -kernel kunxcore/kernel.img -nographic -no-reboot \
    -netdev user,id=n0,hostfwd=tcp::18080-:8080,hostfwd=udp::19090-:9090 \
    -device virtio-net-device,netdev=n0
```

等待系统启动（networkd 完成初始化），进入 `kunxos$` shell 后：

## 基础网络命令

| 命令 | 说明 | 示例 |
|---|---|---|
| `ping <ip>` | ICMP echo | `ping 10.0.2.2`（网关） |
| `nslookup <域名>` | DNS 解析 | `nslookup example.com` |
| `netstat` | 查看套接字表 | `netstat` |

## 服务器演示（宿主机访问）

### TCP 回显服务器

```sh
kunxos$ tcp_echo 8080
# 宿主机：
nc 127.0.0.1 18080        # 输入任意文本，原样返回
```

### UDP 回显服务器

```sh
kunxos$ udp_echo 9090
# 宿主机：
echo -n hello | nc -u -w1 127.0.0.1 19090   # 应回显 hello
```

### HTTP 静态网页服务器

内置欢迎页：

```sh
kunxos$ http_serve 8080
# 宿主机：
curl http://127.0.0.1:18080/
```

提供文件系统目录（`/var/www` 已预置 `index.html`）：

```sh
kunxos$ http_serve 8080 /var/www
# 宿主机：
curl http://127.0.0.1:18080/          # → index.html
curl http://127.0.0.1:18080/index.html
curl -I http://127.0.0.1:18080/nope   # → 404
```

## 网络栈能力

- **TCP**：客户端主动连接 + 服务器 listen/accept（三次握手、序号回绕安全、
  FIN 重传、部分重传补入队）
- **UDP**：收发 + 自动临时端口
- **ICMP**：ping
- **DNS**：A 记录解析（经 QEMU slirp → 宿主 DNS）
- **接收端校验和验证**：损坏包自动丢弃
