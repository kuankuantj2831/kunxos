// 静态链接的 ARM64 用户态测试程序（Phase 8 ELF 演示）。
// Linux AArch64 系统调用约定：x8 = 系统调用号，x0-x5 = 参数，svc #0。
    .text
    .global _start
    .type _start, %function
_start:
    // write(1, "HELLO-FROM-APP\n", 15)
    mov x0, #1
    adr x1, msg
    mov x2, #15
    mov x8, #64                 // __NR_write = 64
    svc #0

    // write(1, "BYE\n", 4) —— 验证连续两次系统调用
    mov x0, #1
    adr x1, bye
    mov x2, #4
    mov x8, #64
    svc #0

    // exit(0)
    mov x0, #0
    mov x8, #93                 // __NR_exit = 93
    svc #0

    b .

msg:
    .ascii "HELLO-FROM-APP\n"
bye:
    .ascii "BYE\n"
