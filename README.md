# Rust simple e1000 device driver (Intel ethernet adapter)

This is for figuring out proper Rust PCI, DMA, network abstraction APIs for NIC drivers.

I implemented abstraction APIs (PCI, DMA, network, etc) for minimum functionality. No `unsafe` for calling C APIs in the driver. I've been working for upstreaming.
Meanwhile you can compile the driver with [my fork of Linux kernel](https://github.com/fujita/linux/tree/rust-e1000).

```bash
$ make KDIR=~/git/linux LLVM=1
```

This driver works on QEMU, howerver nothing else works.


FYI, my command line is
```text
qemu-system-x86_64 -smp 2 -m 1G \
  -kernel "arch/x86_64/boot/bzImage" \
  -initrd ../../initrd.img \
  -nographic -vga none \
  -append console=ttyS0 \
  -no-reboot -nic tap \
  -virtfs local,path=/home/ubuntu/git/quinn,mount_tag=quinn,security_model=none
```

## how to configure the tap device on the host

1. Check whether there is a tap device with `ifconfig`. If not, run the following command to add the tap0 device.

```
tunctl -t tap0 -u `whoami`
ip addr add 192.168.1.1/24 dev tap0
ifconfig tap0 up
```

2. Make sure you configure the route right in the qemu file system.
