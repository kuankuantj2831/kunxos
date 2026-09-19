// 静态链接的 RISC-V 64 用户态测试程序（Phase 8 ELF 演示）。
// Linux riscv64 系统调用约定：a7 = 系统调用号，a0-a5 = 参数，ecall。
    .text
    .global _start
    .type _start, %function
_start:
    // write(1, "HELLO-FROM-APP\n", 15)
    li a0, 1
    la a1, msg
    li a2, 15
    li a7, 64                 // __NR_write = 64
    ecall

    // write(1, "BYE\n", 4) —— 验证连续两次系统调用
    li a0, 1
    la a1, bye
    li a2, 4
    li a7, 64
    ecall

    // exit(0)
    li a0, 0
    li a7, 93                 // __NR_exit = 93
    ecall

    j .

msg:
    .ascii "HELLO-FROM-APP\n"
bye:
    .ascii "BYE\n"
