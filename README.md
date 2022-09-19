# Rust simple e1000 device driver (Intel ethernet adapter)

This is for figuring out proper Rust PCI, DMA, network abstraction APIs for NIC drivers.
    
This driver works on QEMU, howerver nothing else works.

[Rust-for-Linux tree](https://github.com/Rust-for-Linux/linux) doesn't support necessary abstraction APIs (PCI, DMA, network, etc) yet. This driver is tested with [my fork](https://github.com/fujita/linux/tree/rust-e1000). I'll work for upstreaming.

```bash
$ make KDIR=~/git/linux LLVM=1
```
