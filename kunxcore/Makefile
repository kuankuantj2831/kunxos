# 内核目录入口统一转发到顶层 RISC-V 构建，避免维护第二套启动参数。
.PHONY: all build run run-gui run-net clean
all build:
	$(MAKE) -C .. kernel
run run-gui clean:
	$(MAKE) -C .. $@
run-net:
	$(MAKE) -C .. run-gui
