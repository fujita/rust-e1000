// SPDX-License-Identifier: GPL-2.0

//! Rust simple Intel e1000 driver (works on only QEMU)

#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::ffi::c_void;
use kernel::bindings;
use kernel::c_str;
use kernel::device;
use kernel::dma;
use kernel::driver;
use kernel::irq;
use kernel::net;
use kernel::pci;
use kernel::prelude::*;
use kernel::sync::{Ref, UniqueRef};

mod e1000_defs;
use e1000_defs::*;

mod ops;
use ops::*;

struct Napi {
    inner: UnsafeCell<bindings::napi_struct>,
}

impl Napi {
    fn enable(&self) {
        unsafe {
            bindings::napi_enable(self.inner.get());
        }
    }

    fn schedule(&self) {
        unsafe {
            if bindings::napi_schedule_prep(self.inner.get()) {
                bindings::__napi_schedule(self.inner.get())
            }
        }
    }
}

struct Resource {
    mmio: pci::MappedResource,
    io_base: u64,
}

impl Resource {
    fn er32(&self, reg: u32) -> u32 {
        self.mmio.readl(reg as usize).unwrap()
    }

    fn ew32(&self, reg: u32, value: u32) {
        let _ = self.mmio.writel(value, reg as usize);
    }

    fn write_flush(&self) {
        self.er32(E1000_STATUS);
    }

    fn write_reg_io(&self, offset: u32, value: u32) {
        unsafe {
            bindings::outl(offset, self.io_base);
            bindings::outl(value, self.io_base + 4);
        }
    }

    fn write_reg_array(&self, reg: u32, offset: u32, value: u32) {
        let _ = self
            .mmio
            .writel(value, reg as usize + (offset << 2) as usize);
    }
}

#[derive(Clone, Copy)]
struct Buffer {
    skb: Option<*mut bindings::sk_buff>,
    dma: u64,
    length: usize,
}

const DEFAULT_TXD: usize = 128;
const RXBUFFER_2048: u32 = 2048;

struct Ring {
    buffer_info: [Buffer; DEFAULT_TXD],
    desc: *mut c_void,
    dma_addr: u64,
    head_offset: usize,
    tail_offset: usize,
    next_to_use: u16,
    next_to_clean: u16,
}

impl Ring {
    fn new(dev: &device::Device, desc_size: usize) -> Ring {
        let mut size = DEFAULT_TXD * desc_size;
        size = (size + 4096 - 1) & !(4096 - 1);
        let mut dma_addr = 0;
        let desc = dma::alloc_coherent(dev, size as usize, &mut dma_addr, bindings::GFP_KERNEL)
            .unwrap();
        Ring {
            buffer_info: [Buffer {
                skb: None,
                dma: 0,
                length: 0,
            }; DEFAULT_TXD],
            desc,
            dma_addr,
            head_offset: 0,
            tail_offset: 0,
            next_to_use: 0,
            next_to_clean: 0,
        }
    }
}

struct IntrData {
    re: Ref<Resource>,
    napi: Ref<Napi>,
}

impl irq::Handler for E1000 {
    type Data = Box<IntrData>;

    fn handle_irq(data: &IntrData) -> irq::Return {
        let icr = data.re.er32(E1000_ICR);
        if icr == 0 {
            return irq::Return::None;
        }
        data.napi.schedule();

        irq::Return::Handled
    }
}

/// per device Data
struct DevData {
    dev: Ref<device::Device>,
    re: Ref<Resource>,
    irqnum: u32,
    tx_ring: Option<Ring>,
    rx_ring: Option<Ring>,
    napi: Ref<Napi>,
    _irq: Option<irq::Registration<E1000>>,
}

const E1000_TX_FLAGS_IPV4: u32 = 0x00000008;

impl DeviceOperations for E1000 {
    fn open(netdev: *mut bindings::net_device) -> i32 {
        let mut devdata: Box<DevData> =
            unsafe { Box::from_raw(bindings::dev_get_drvdata(&mut (*netdev).dev) as _) };

        let mut tx_ring = Ring::new(&devdata.dev, core::mem::size_of::<TxDesc>());
        let mut rx_ring = Ring::new(&devdata.dev, core::mem::size_of::<RxDesc>());
        E1000::power_up_phy(&devdata.re);

        E1000::configure(
            netdev,
            &devdata.dev,
            &devdata.re,
            &mut tx_ring,
            &mut rx_ring,
        );

        devdata.tx_ring.replace(tx_ring);
        devdata.rx_ring.replace(rx_ring);

        let intrdata = Box::try_new(IntrData {
            re: devdata.re.clone(),
            napi: devdata.napi.clone(),
        })
        .unwrap();

        devdata.napi.enable();

        let irq =
            irq::Registration::try_new(devdata.irqnum, intrdata, irq::flags::SHARED, fmt!("e1000"))
                .unwrap();
        devdata._irq.replace(irq);

        E1000::irq_enable(&devdata.re);
        unsafe {
            bindings::netif_start_queue(netdev);
        }

        /* fire a link status change interrupt to start the watchdog */
        devdata.re.ew32(E1000_ICS, E1000_ICS_LSC);

        // HACK; should do watchdog handler
        unsafe {
            bindings::netif_carrier_on(netdev);
            let mut tctl = devdata.re.er32(E1000_TCTL);
            tctl |= E1000_TCTL_EN;
            devdata.re.ew32(E1000_TCTL, tctl);

            let rctl = devdata.re.er32(E1000_RCTL);
            devdata.re.ew32(E1000_RCTL, rctl | E1000_RCTL_EN);
            devdata.re.ew32(E1000_ICS, E1000_ICS_RXDMT0);
        }

        unsafe {
            bindings::dev_set_drvdata(
                &mut (*netdev).dev,
                Box::into_raw(devdata) as *const _ as *mut c_void,
            );
        }
        0
    }

    fn xmit(skb: *mut bindings::sk_buff, netdev: *mut bindings::net_device) -> i32 {
        let mut devdata: Box<DevData> =
            unsafe { Box::from_raw(bindings::dev_get_drvdata(&mut (*netdev).dev) as _) };

        unsafe {
            let mut tx_ring = &mut devdata.tx_ring.as_mut().unwrap();
            let mut _tx_flags = 0;
            bindings::skb_put_padto(skb, bindings::ETH_ZLEN);
            let mss = (*bindings::skb_shinfo(skb)).gso_size;
            assert!(mss == 0);

            let nr_frags = (*bindings::skb_shinfo(skb)).nr_frags;
            assert!(nr_frags == 0);

            if bindings::vlan_get_protocol(skb) == 0x0008 {
                _tx_flags |= E1000_TX_FLAGS_IPV4;
            }

            let mut i = tx_ring.next_to_use as usize;
            let size = (*skb).len - (*skb).data_len;
            let dev = &devdata.dev;
            let info = &mut tx_ring.buffer_info[i];

            let dma = dma::map_single(
                dev,
                (*skb).data as _,
                size as usize,
                bindings::dma_data_direction_DMA_TO_DEVICE,
            );
            assert!(dma != !0);

            info.skb = Some(skb);
            info.dma = dma;
            info.length = size as usize;

            bindings::netdev_sent_queue(netdev, (*skb).len);

            let desc = tx_ring.desc.add(i * core::mem::size_of::<TxDesc>()) as *mut TxDesc;
            (*desc).buffer_addr = dma;
            (*desc).lower.data = E1000_TXD_CMD_IFCS | size as u32;
            (*desc).upper.data = 0;
            (*desc).lower.data |= E1000_TXD_CMD_EOP | E1000_TXD_CMD_IFCS | E1000_TXD_CMD_RS;

            i += 1;
            if i == DEFAULT_TXD {
                i = 0;
            }
            tx_ring.next_to_use = i as u16;
            devdata.re.ew32(E1000_TDT, i as u32);
        }

        unsafe {
            bindings::dev_set_drvdata(
                &mut (*netdev).dev,
                Box::into_raw(devdata) as *const _ as *mut c_void,
            );
        }

        0
    }

    fn set_rx_mode(netdev: *mut bindings::net_device) {
        let devdata: Box<DevData> =
            unsafe { Box::from_raw(bindings::dev_get_drvdata(&mut (*netdev).dev) as _) };

        E1000::set_rx_mode(netdev, &devdata.re);

        unsafe {
            bindings::dev_set_drvdata(
                &mut (*netdev).dev,
                Box::into_raw(devdata) as *const _ as *mut c_void,
            );
        }
    }
}

struct EepromInfo {
    word_size: u16,
    opcode_bits: u16,
    address_bits: u16,
    delay_usec: u16,
}

impl E1000 {
    unsafe extern "C" fn clean(napi: *mut bindings::napi_struct, _budget: i32) -> i32 {
        let mut devdata: Box<DevData> =
            unsafe { Box::from_raw(bindings::dev_get_drvdata(&mut (*(*napi).dev).dev) as _) };

        {
            let cleaned_count =
                E1000::clean_rx_irq(&devdata.dev, napi, &mut devdata.rx_ring.as_mut().unwrap());
            unsafe {
                E1000::alloc_rx_buffers(
                    (*napi).dev,
                    &devdata.dev,
                    &devdata.re,
                    &mut devdata.rx_ring.as_mut().unwrap(),
                    cleaned_count as u16,
                );
            }
            E1000::clean_tx_irq(&devdata.dev, napi, &mut devdata.tx_ring.as_mut().unwrap());
        }

        unsafe {
            bindings::napi_complete_done(napi, 1);
        }

        unsafe {
            bindings::dev_set_drvdata(
                &mut (*(*napi).dev).dev,
                Box::into_raw(devdata) as *const _ as *mut c_void,
            );
        }

        1
    }

    fn clean_tx_irq(dev: &device::Device, napi: *mut bindings::napi_struct, tx_ring: &mut Ring) {
        let mut i = tx_ring.next_to_clean as usize;
        let mut pkts_compl = 0;
        let mut bytes_compl = 0;

        unsafe {
            let mut desc = tx_ring.desc.add(i * core::mem::size_of::<TxDesc>()) as *mut TxDesc;

            while (*desc).upper.data & E1000_TXD_STAT_DD != 0 {
                let mut info = &mut tx_ring.buffer_info[i];

                dma::unmap_single(
                    dev,
                    info.dma,
                    info.length,
                    bindings::dma_data_direction_DMA_TO_DEVICE,
                );
                info.dma = 0;
                let skb = info.skb.take().unwrap();
                (*desc).upper.data = 0;
                pkts_compl += 1;
                bytes_compl += (*skb).len;

                bindings::napi_consume_skb(skb, 64);

                i += 1;
                if i == DEFAULT_TXD {
                    i = 0;
                }
                desc = tx_ring.desc.add(i * core::mem::size_of::<TxDesc>()) as *mut TxDesc;
            }

            (*(*napi).dev).stats.tx_bytes += bytes_compl as u64;
            (*(*napi).dev).stats.tx_packets += pkts_compl as u64;
        }
        tx_ring.next_to_clean = i as u16;

        unsafe {
            bindings::netdev_completed_queue((*napi).dev, pkts_compl, bytes_compl);
        }
    }

    fn clean_rx_irq(
        dev: &device::Device,
        napi: *mut bindings::napi_struct,
        rx_ring: &mut Ring,
    ) -> usize {
        let mut i = rx_ring.next_to_clean as usize;
        let mut pkts_compl = 0;
        let mut bytes_compl = 0;

        unsafe {
            let mut desc = rx_ring.desc.add(i * core::mem::size_of::<RxDesc>()) as *mut RxDesc;
            while (*desc).status & E1000_RXD_STAT_DD as u8 != 0 {
                let mut length = (*desc).length;
                let mut info = &mut rx_ring.buffer_info[i];

                dma::unmap_single(
                    dev,
                    info.dma,
                    RXBUFFER_2048 as usize,
                    bindings::dma_data_direction_DMA_FROM_DEVICE,
                );
                info.dma = 0;
                let skb = info.skb.take().unwrap();

                length -= 4;
                bindings::skb_put(skb, length as u32);
                bindings::skb_set_protocol(skb, bindings::eth_type_trans(skb, (*napi).dev));
                bindings::napi_gro_receive(napi, skb);
                (*desc).status = 0;
                (*desc).buffer_addr = 0;

                pkts_compl += 1;
                bytes_compl += length;

                i += 1;
                if i == DEFAULT_TXD {
                    i = 0;
                }
                desc = rx_ring.desc.add(i * core::mem::size_of::<RxDesc>()) as *mut RxDesc;
            }

            (*(*napi).dev).stats.rx_bytes += bytes_compl as u64;
            (*(*napi).dev).stats.rx_packets += pkts_compl as u64;
        }
        rx_ring.next_to_clean = i as u16;
        pkts_compl
    }

    fn configure(
        netdev: *mut bindings::net_device,
        dev: &device::Device,
        re: &Resource,
        tx_ring: &mut Ring,
        rx_ring: &mut Ring,
    ) {
        E1000::set_rx_mode(netdev, re);
        E1000::configure_tx(re, tx_ring);
        E1000::setup_rctl(netdev, re);
        E1000::configure_rx(re, rx_ring);
        let cleaned_count = E1000::unused_desc(rx_ring);
        E1000::alloc_rx_buffers(netdev, dev, re, rx_ring, cleaned_count);
    }

    fn configure_tx(re: &Resource, tx_ring: &Ring) {
        let tdba = tx_ring.dma_addr;
        let tdlen = DEFAULT_TXD * core::mem::size_of::<TxDesc>();
        re.ew32(E1000_TDLEN, tdlen as u32);
        re.ew32(E1000_TDBAH, (tdba >> 32) as u32);
        re.ew32(E1000_TDBAL, (tdba & 0x00000000ffffffff) as u32);
        re.ew32(E1000_TDT, 0);
        re.ew32(E1000_TDH, 0);
        // adapter->tx_ring[0].tdh = ((hw->mac_type >= e1000_82543) ?
        //                            E1000_TDH : E1000_82542_TDH);
        // adapter->tx_ring[0].tdt = ((hw->mac_type >= e1000_82543) ?
        //                            E1000_TDT : E1000_82542_TDT);

        let mut tipg = DEFAULT_82543_TIPG_IPGT_COPPER;
        let ipgr1 = DEFAULT_82543_TIPG_IPGR1;
        let ipgr2 = DEFAULT_82543_TIPG_IPGR2;

        tipg |= ipgr1 << E1000_TIPG_IPGR1_SHIFT;
        tipg |= ipgr2 << E1000_TIPG_IPGR2_SHIFT;
        re.ew32(E1000_TIPG, tipg);

        /* Set the Tx Interrupt Delay register */
        re.ew32(E1000_TIDV, 0);
        re.ew32(E1000_TADV, 0);

        // adapter->txd_cmd = E1000_TXD_CMD_EOP | E1000_TXD_CMD_IFCS;
        // adapter->txd_cmd |= E1000_TXD_CMD_RS;

        let mut tctl = re.er32(E1000_TCTL);
        tctl &= !E1000_TCTL_CT;
        tctl |= E1000_TCTL_PSP | E1000_TCTL_RTLC | (E1000_COLLISION_THRESHOLD << E1000_CT_SHIFT);
        re.ew32(E1000_TCTL, tctl);
    }

    fn setup_rctl(netdev: *mut bindings::net_device, re: &Resource) {
        let mut rctl = re.er32(E1000_RCTL);

        rctl &= !(3 << E1000_RCTL_MO_SHIFT);

        rctl |= E1000_RCTL_BAM | E1000_RCTL_LBM_NO | E1000_RCTL_RDMTS_HALF;
        //(hw->mc_filter_type << E1000_RCTL_MO_SHIFT);

        rctl &= !E1000_RCTL_SBP;
        unsafe { assert!((*netdev).mtu <= bindings::ETH_DATA_LEN) }
        rctl &= !E1000_RCTL_LPE;

        rctl &= !E1000_RCTL_SZ_4096;
        rctl |= E1000_RCTL_SZ_2048;
        rctl &= !E1000_RCTL_BSEX;
        re.ew32(E1000_RCTL, rctl);
    }

    fn unused_desc(ring: &Ring) -> u16 {
        // reserve one
        if ring.next_to_clean > ring.next_to_use {
            ring.next_to_clean - ring.next_to_use - 1
        } else {
            DEFAULT_TXD as u16 + ring.next_to_clean - ring.next_to_use - 1
        }
    }

    fn alloc_rx_buffers(
        netdev: *mut bindings::net_device,
        dev: &device::Device,
        re: &Resource,
        ring: &mut Ring,
        cleaned_count: u16,
    ) {
        let len = RXBUFFER_2048;
        let mut i = ring.next_to_use as usize;
        for _ in 0..cleaned_count {
            let info = &mut ring.buffer_info[i];
            assert!(info.skb.is_none());
            unsafe {
                let skb = bindings::netdev_alloc_skb_ip_align(netdev, len);
                let dma = dma::map_single(
                    dev,
                    (*skb).data as _,
                    len as usize,
                    bindings::dma_data_direction_DMA_FROM_DEVICE,
                );
                assert!(dma != !0);
                let desc = ring.desc.add(i * core::mem::size_of::<RxDesc>()) as *mut RxDesc;
                (*desc).buffer_addr = dma;
                info.dma = dma;
                info.skb.replace(skb);
            }
            i += 1;
            if i == DEFAULT_TXD {
                i = 0;
            }
        }
        if i as u16 != ring.next_to_use {
            ring.next_to_use = i as u16;
            if i == 0 {
                i = DEFAULT_TXD - 1;
            } else {
                i -= 1;
            }
            re.ew32(E1000_RDT, i as u32);
        }
    }

    fn configure_rx(re: &Resource, rx_ring: &Ring) {
        // adapter->clean_rx = e1000_clean_rx_irq;
        // adapter->alloc_rx_buf = e1000_alloc_rx_buffers;

        /* disable receives while setting up the descriptors */
        let rctl = re.er32(E1000_RCTL);
        re.ew32(E1000_RCTL, rctl & !E1000_RCTL_EN);

        /* set the Receive Delay Timer Register */
        re.ew32(E1000_RDTR, 0);
        re.ew32(E1000_RADV, 8);
        // adapter->itr = 3
        re.ew32(E1000_ITR, 1000000000 / (3 * 256));

        let rdba = rx_ring.dma_addr;
        let rdlen = DEFAULT_TXD * core::mem::size_of::<RxDesc>();
        re.ew32(E1000_RDLEN, rdlen as u32);
        re.ew32(E1000_RDBAH, (rdba >> 32) as u32);
        re.ew32(E1000_RDBAL, (rdba & 0x00000000ffffffff) as u32);
        re.ew32(E1000_RDT, 0);
        re.ew32(E1000_RDH, 0);
        // adapter->rx_ring[0].rdh = ((hw->mac_type >= e1000_82543) ?
        //                            E1000_RDH : E1000_82542_RDH);
        // adapter->rx_ring[0].rdt = ((hw->mac_type >= e1000_82543) ?
        //                            E1000_RDT : E1000_82542_RDT);
        /* Enable Receives */
        re.ew32(E1000_RCTL, rctl | E1000_RCTL_EN);
    }

    fn set_rx_mode(netdev: *mut bindings::net_device, re: &Resource) {
        let mut rctl = re.er32(E1000_RCTL);

        unsafe {
            assert!((*netdev).flags & bindings::net_device_flags_IFF_PROMISC == 0);
            assert!((*netdev).uc.count == 0);

            if (*netdev).flags & bindings::net_device_flags_IFF_ALLMULTI > 0 {
                rctl |= E1000_RCTL_MPE;
            } else {
                rctl &= !E1000_RCTL_MPE;
            }
        }

        rctl &= !E1000_RCTL_UPE;
        re.ew32(E1000_RCTL, rctl);

        for i in 1..E1000_RAR_ENTRIES {
            re.ew32(E1000_RA + ((i << 1) << 2), 0);
            re.write_flush();
            re.ew32(E1000_RA + (((i << 1) + 1) << 2), 0);
            re.write_flush();
        }

        let mut i = E1000_NUM_MTA_REGISTERS - 1;
        loop {
            re.ew32(E1000_MTA + i << 2, 0);
            if i == 0 {
                break;
            }
            i -= 1;
        }
        re.write_flush();
    }

    fn bar0_length(pdev: &pci::Device) -> usize {
        for r in pdev.iter_resource() {
            return r.len() as usize;
        }
        assert!(false);
        0
    }

    fn io_base(pdev: &pci::Device) -> u64 {
        for (i, r) in pdev.iter_resource().enumerate() {
            if i == 0 || r.len() == 0 {
                continue;
            }
            if r.flags & bindings::IORESOURCE_IO as u64 > 0 {
                return r.start;
            }
        }
        assert!(false);
        0
    }

    fn init_eeprom_params(re: &Resource) -> EepromInfo {
        // eeprom->type = e1000_eeprom_microwire;

        let eecd = re.er32(E1000_EECD);
        if eecd & E1000_EECD_SIZE > 0 {
            EepromInfo {
                word_size: 256,
                address_bits: 8,
                delay_usec: 50,
                opcode_bits: 3,
            }
        } else {
            EepromInfo {
                word_size: 64,
                address_bits: 6,
                delay_usec: 50,
                opcode_bits: 3,
            }
        }
    }

    fn acquire_eeprom(re: &Resource, _eeprom: &EepromInfo) {
        let mut eecd = re.er32(E1000_EECD);

        // Request EEPROM Access
        eecd |= E1000_EECD_REQ;
        re.ew32(E1000_EECD, eecd);
        eecd = re.er32(E1000_EECD);

        let mut i = 0;
        while eecd & E1000_EECD_GNT == 0 && i < E1000_EEPROM_GRANT_ATTEMPTS {
            i += 1;
            unsafe { bindings::__const_udelay(5) }
            eecd = re.er32(E1000_EECD);
        }

        assert!(eecd & E1000_EECD_GNT > 0);

        /* Clear SK and DI */
        eecd &= !(E1000_EECD_DI | E1000_EECD_SK);
        re.ew32(E1000_EECD, eecd);

        /* Set CS */
        eecd |= E1000_EECD_CS;
        re.ew32(E1000_EECD, eecd);

        re.write_flush();
        unsafe { bindings::__const_udelay(1) }
    }

    fn release_eeprom(re: &Resource, eeprom: &EepromInfo) {
        let mut eecd = re.er32(E1000_EECD);

        eecd &= !(E1000_EECD_CS | E1000_EECD_DI);

        re.ew32(E1000_EECD, eecd);

        /* Rising edge of clock */
        eecd |= E1000_EECD_SK;
        re.ew32(E1000_EECD, eecd);
        re.write_flush();
        unsafe {
            bindings::__udelay(eeprom.delay_usec.into());
        }

        /* Falling edge of clock */
        eecd &= !E1000_EECD_SK;
        re.ew32(E1000_EECD, eecd);
        re.write_flush();
        unsafe {
            bindings::__udelay(eeprom.delay_usec.into());
        }

        eecd &= !E1000_EECD_REQ;
        re.ew32(E1000_EECD, eecd);
    }

    fn raise_ee_clk(re: &Resource, eeprom: &EepromInfo, eecd: &mut u32) {
        /* Raise the clock input to the EEPROM (by setting the SK bit), and then
         * wait <delay> microseconds.
         */
        *eecd = *eecd | E1000_EECD_SK;
        re.ew32(E1000_EECD, *eecd);
        re.write_flush();
        unsafe {
            bindings::__udelay(eeprom.delay_usec.into());
        }
    }

    fn lower_ee_clk(re: &Resource, eeprom: &EepromInfo, eecd: &mut u32) {
        /* Lower the clock input to the EEPROM (by clearing the SK bit), and
         * then wait 50 microseconds.
         */
        *eecd = *eecd & !E1000_EECD_SK;
        re.ew32(E1000_EECD, *eecd);
        re.write_flush();
        unsafe {
            bindings::__udelay(eeprom.delay_usec.into());
        }
    }

    fn shift_out_ee_bits(re: &Resource, eeprom: &EepromInfo, data: u16, count: u16) {
        let mut mask = 0x01 << (count - 1);
        let mut eecd = re.er32(E1000_EECD);

        eecd &= !E1000_EECD_DO;

        loop {
            eecd &= !E1000_EECD_DI;

            if data & mask > 0 {
                eecd |= E1000_EECD_DI;
            }

            let _ = re.ew32(E1000_EECD, eecd);
            re.write_flush();

            unsafe {
                bindings::__udelay(eeprom.delay_usec.into());
            }

            E1000::raise_ee_clk(re, eeprom, &mut eecd);
            E1000::lower_ee_clk(re, eeprom, &mut eecd);

            mask = mask >> 1;

            if mask == 0 {
                break;
            }
        }

        eecd &= !E1000_EECD_DI;
        re.ew32(E1000_EECD, eecd);
    }

    fn shift_in_ee_bits(re: &Resource, eeprom: &EepromInfo, count: u16) -> u16 {
        /* In order to read a register from the EEPROM, we need to shift 'count'
         * bits in from the EEPROM. Bits are "shifted in" by raising the clock
         * input to the EEPROM (setting the SK bit), and then reading the value
         * of the "DO" bit.  During this "shifting in" process the "DI" bit
         * should always be clear.
         */
        let mut eecd = re.er32(E1000_EECD);

        eecd &= !(E1000_EECD_DO | E1000_EECD_DI);
        let mut data = 0;

        for _ in 0..count {
            data = data << 1;
            E1000::raise_ee_clk(re, eeprom, &mut eecd);

            eecd = re.er32(E1000_EECD);

            eecd &= !E1000_EECD_DI;
            if eecd & E1000_EECD_DO > 0 {
                data |= 1;
            }
            E1000::lower_ee_clk(re, eeprom, &mut eecd);
        }

        data
    }

    fn standby_eeprom(re: &Resource, eeprom: &EepromInfo) {
        let mut eecd = re.er32(E1000_EECD);

        eecd &= !(E1000_EECD_CS | E1000_EECD_SK);
        let _ = re.ew32(E1000_EECD, eecd);
        re.write_flush();
        unsafe {
            bindings::__udelay(eeprom.delay_usec.into());
        }

        /* Clock high */
        eecd |= E1000_EECD_SK;
        let _ = re.ew32(E1000_EECD, eecd);
        re.write_flush();
        unsafe {
            bindings::__udelay(eeprom.delay_usec.into());
        }

        /* Select EEPROM */
        eecd |= E1000_EECD_CS;
        let _ = re.ew32(E1000_EECD, eecd);
        re.write_flush();
        unsafe {
            bindings::__udelay(eeprom.delay_usec.into());
        }

        /* Clock low */
        eecd &= !E1000_EECD_SK;
        let _ = re.ew32(E1000_EECD, eecd);
        re.write_flush();
        unsafe {
            bindings::__udelay(eeprom.delay_usec.into());
        }
    }

    fn e1000_read_eeprom(re: &Resource, eeprom: &EepromInfo, offset: u16, words: u16) -> u16 {
        assert!(offset < eeprom.word_size);
        assert!(words == 1);
        assert!(words <= eeprom.word_size - offset);
        E1000::acquire_eeprom(re, eeprom);

        let mut data = 0;
        for i in 0..words {
            /* Send the READ command (opcode + addr)  */
            E1000::shift_out_ee_bits(
                re,
                eeprom,
                EEPROM_READ_OPCODE_MICROWIRE as u16,
                eeprom.opcode_bits,
            );
            E1000::shift_out_ee_bits(re, eeprom, offset + i, eeprom.address_bits);

            /* Read the data.  For microwire, each word requires the
             * overhead of eeprom setup and tear-down.
             */
            data = E1000::shift_in_ee_bits(re, eeprom, 16);
            E1000::standby_eeprom(re, eeprom);
            unsafe {
                bindings::cond_resched();
            }
        }

        E1000::release_eeprom(re, eeprom);
        data
    }

    fn read_mac_addr(re: &Resource, eeprom: &EepromInfo) -> [u8; NODE_ADDRESS_SIZE as usize] {
        let mut perm_mac_addr = [0u8; NODE_ADDRESS_SIZE as usize];

        for i in 0..NODE_ADDRESS_SIZE {
            if i % 2 > 0 {
                continue;
            }
            let eeprom_data = E1000::e1000_read_eeprom(re, eeprom, (i >> 1) as u16, 1);
            perm_mac_addr[i as usize] = (eeprom_data & 0x00ff) as u8;
            perm_mac_addr[i as usize + 1] = (eeprom_data >> 8) as u8;
        }

        perm_mac_addr
    }

    fn validate_eeprom_checksum(re: &Resource, eeprom: &EepromInfo) {
        let mut checksum: u16 = 0;
        for i in 0..EEPROM_CHECKSUM_REG + 1 {
            let eeprom_data = E1000::e1000_read_eeprom(re, eeprom, i as u16, 1);
            checksum = checksum.wrapping_add(eeprom_data);
        }
        if checksum != EEPROM_SUM as u16 {
            pr_info!("checksum doesn't match: {} {}", checksum, EEPROM_SUM);
        }
    }

    fn reset_hw(re: &Resource) {
        re.ew32(E1000_IMC, 0xffffffff);

        re.ew32(E1000_RCTL, 0);
        re.ew32(E1000_TCTL, E1000_TCTL_PSP);
        re.write_flush();

        unsafe {
            bindings::msleep(10);
        }
        let ctrl = re.er32(E1000_CTRL);

        re.write_reg_io(E1000_CTRL, ctrl | E1000_CTRL_RST);
        unsafe {
            bindings::msleep(5);
        }

        let mut manc = re.er32(E1000_MANC);
        manc &= !E1000_MANC_ARP_EN;
        re.ew32(E1000_MANC, manc);

        re.ew32(E1000_IMC, 0xffffffff);

        /* Clear any pending interrupt events. */
        re.er32(E1000_ICR);
    }

    fn irq_enable(re: &Resource) {
        re.ew32(E1000_IMS, E1000_IMS_ENABLE_MASK);
        re.write_flush();
    }

    fn irq_disable(re: &Resource) {
        re.ew32(E1000_IMC, !0);
        re.write_flush();
    }

    fn sw_init(re: &Resource) {
        E1000::irq_disable(re);
    }

    fn raise_mdi_clk(re: &Resource, ctrl: &mut u32) {
        re.ew32(E1000_CTRL, *ctrl | E1000_CTRL_MDC);
        re.write_flush();
        unsafe {
            bindings::__const_udelay(10);
        }
    }

    fn lower_mdi_clk(re: &Resource, ctrl: &mut u32) {
        re.ew32(E1000_CTRL, *ctrl & !E1000_CTRL_MDC);
        re.write_flush();
        unsafe {
            bindings::__const_udelay(10);
        }
    }

    fn shift_in_mdi_bits(re: &Resource) -> u16 {
        let mut data = 0;

        let mut ctrl = re.er32(E1000_CTRL);
        ctrl &= !E1000_CTRL_MDIO_DIR;
        ctrl &= !E1000_CTRL_MDIO;

        re.ew32(E1000_CTRL, ctrl);
        re.write_flush();

        E1000::raise_mdi_clk(re, &mut ctrl);
        E1000::lower_mdi_clk(re, &mut ctrl);

        for _ in 0..16 {
            data = data << 1;
            E1000::raise_mdi_clk(re, &mut ctrl);
            ctrl = re.er32(E1000_CTRL);
            if ctrl & E1000_CTRL_MDIO > 0 {
                data |= 1;
            }
            E1000::lower_mdi_clk(re, &mut ctrl);
        }
        E1000::raise_mdi_clk(re, &mut ctrl);
        E1000::lower_mdi_clk(re, &mut ctrl);

        data
    }

    fn shift_out_mdi_bits(re: &Resource, data: u32, count: u16) {
        let mut mask = 0x01;
        mask <<= count - 1;

        let mut ctrl = re.er32(E1000_CTRL);
        ctrl |= E1000_CTRL_MDIO_DIR | E1000_CTRL_MDC_DIR;

        while mask != 0 {
            if data & mask > 0 {
                ctrl |= E1000_CTRL_MDIO;
            } else {
                ctrl &= !E1000_CTRL_MDIO;
            }

            re.ew32(E1000_CTRL, ctrl);
            re.write_flush();
            unsafe {
                bindings::__const_udelay(10);
            }

            E1000::raise_mdi_clk(re, &mut ctrl);
            E1000::lower_mdi_clk(re, &mut ctrl);

            mask = mask >> 1;
        }
    }

    fn write_phy_reg(re: &Resource, reg_addr: u32, phy_data: u16) {
        assert!(reg_addr <= MAX_PHY_REG_ADDRESS);
        let phy_addr = 1;

        let mut mdic = phy_data as u32
            | reg_addr << E1000_MDIC_REG_SHIFT
            | phy_addr << E1000_MDIC_PHY_SHIFT
            | E1000_MDIC_OP_WRITE;

        re.ew32(E1000_MDIC, mdic);

        for _ in 0..641 {
            unsafe {
                bindings::__const_udelay(5);
            }
            mdic = re.er32(E1000_MDIC);
            if mdic & E1000_MDIC_READY > 0 {
                break;
            }

            assert!(mdic & E1000_MDIC_READY > 0);
        }
    }

    fn read_phy_reg(re: &Resource, reg_addr: u32) -> u16 {
        let phy_addr = 1;
        E1000::shift_out_mdi_bits(re, PHY_PREAMBLE, PHY_PREAMBLE_SIZE as u16);
        let mdic = (reg_addr) | (phy_addr << 5) | (PHY_OP_READ << 10) | (PHY_SOF << 12);

        E1000::shift_out_mdi_bits(re, mdic, 14);

        E1000::shift_in_mdi_bits(re)
    }

    fn power_up_phy(re: &Resource) {
        // QEMU phy_type = e1000_phy_m88 (0)
        let mut mii_reg = E1000::read_phy_reg(re, PHY_CTRL);
        mii_reg &= !MII_CR_POWER_DOWN as u16;
        E1000::write_phy_reg(re, PHY_CTRL, mii_reg);
    }

    fn rar_set(re: &Resource, mac_addr: &[u8; 6], index: u32) {
        let rar_low = mac_addr[0] as u32
            | (mac_addr[1] as u32) << 8
            | (mac_addr[2] as u32) << 16
            | (mac_addr[3] as u32) << 24;
        let rar_high = mac_addr[4] as u32 | (mac_addr[5] as u32) << 8 | E1000_RAH_AV;

        re.write_reg_array(E1000_RA, index << 1, rar_low);
        re.write_flush();
        re.write_reg_array(E1000_RA, (index << 1) + 1, rar_high);
        re.write_flush();
    }

    fn init_rx_addrs(re: &Resource, mac_addr: &[u8; 6]) {
        E1000::rar_set(re, mac_addr, 0);

        for i in 1..E1000_RAR_ENTRIES {
            re.write_reg_array(E1000_RA, i << 1, 0);
            re.write_flush();
            re.write_reg_array(E1000_RA, (i << 1) + 1, 0);
            re.write_flush();
        }
    }

    fn init_hw(re: &Resource, mac_addr: &[u8; 6]) {
        /* Disabling VLAN filtering. */
        re.ew32(E1000_VET, 0);

        E1000::init_rx_addrs(re, mac_addr);

        /* Zero out the Multicast HASH table */
        for i in 0..E1000_MC_TBL_SIZE {
            re.write_reg_array(E1000_MTA, i, 0);
            /* use write flush to prevent Memory Write Block (MWB) from
             * occurring when accessing our register space
             */
            re.write_flush();
        }

        /* Set the transmit descriptor write-back policy */
        let mut ctrl = re.er32(E1000_TXDCTL);
        ctrl &= !E1000_TXDCTL_WTHRESH | E1000_TXDCTL_FULL_TX_DESC_WB;
        re.ew32(E1000_TXDCTL, ctrl);
    }

    fn reset(re: &Resource, mac_addr: &[u8; 6]) {
        let pba = E1000_PBA_48K;
        re.ew32(E1000_PBA, pba);

        E1000::reset_hw(re);
        E1000::init_hw(re, mac_addr);
    }
}

struct DrvData {
    _dev: net::Device,
}

impl driver::DeviceRemoval for DrvData {
    fn device_remove(&self) {}
}

const DEV_ID_82540EM: u32 = 0x100E;

impl pci::Driver for E1000 {
    type Data = Box<DrvData>;

    fn probe(pdev: &mut pci::Device, _id_info: Option<&Self::IdInfo>) -> Result<Self::Data> {
        pr_info!("intel e1000 probe");

        let bars = pdev.select_bars(
            bindings::IORESOURCE_MEM as core::ffi::c_ulong
                | bindings::IORESOURCE_IO as core::ffi::c_ulong,
        );
        pdev.enable_device()?;
        pdev.request_selected_regions(bars, c_str!("e1000"))?;
        pdev.set_master();

        let mmio = pdev.map_resource(0, E1000::bar0_length(pdev))?;
        let io_base = E1000::io_base(pdev);

        let re = Resource { mmio, io_base };

        let mut ndev = net::Device::with_device(pdev, 1, 1)?;

        // hw->mac_type = e1000_82540

        dma::set_mask(pdev, 0xffffffff)?;
        dma::set_coherent_mask(pdev, 0xffffffff)?;
        E1000::sw_init(&re);

        E1000::reset_hw(&re);

        let eeprom = E1000::init_eeprom_params(&re);
        E1000::validate_eeprom_checksum(&re, &eeprom);
        let mac_addr = E1000::read_mac_addr(&re, &eeprom);
        unsafe {
            bindings::eth_hw_addr_set(ndev.0, &mac_addr as _);

            (*ndev.0).min_mtu = bindings::ETH_MIN_MTU;
            (*ndev.0).max_mtu = MAX_JUMBO_FRAME_SIZE - (bindings::ETH_HLEN + bindings::ETH_FCS_LEN);

            (*ndev.0).priv_flags |= bindings::netdev_priv_flags_IFF_SUPP_NOFCS;

            (*ndev.0).netdev_ops = DeviceOperationsVtable::<E1000>::build();
        }

        E1000::reset(&re, &mac_addr);

        // flan filter off
        let mut rctl = re.er32(E1000_RCTL);
        rctl &= !E1000_RCTL_VFE;
        re.ew32(E1000_RCTL, rctl);

        assert_eq!(core::mem::size_of::<RxDesc>(), 16);
        assert_eq!(core::mem::size_of::<TxDesc>(), 16);

        let mut napi = Pin::from(UniqueRef::try_new(Napi {
            inner: UnsafeCell::new(bindings::napi_struct::default()),
        })?);
        unsafe {
            bindings::netif_napi_add_weight(
                ndev.0,
                (*Pin::as_mut(&mut napi)).inner.get(),
                Some(E1000::clean),
                64,
            );
        }

        let devdata = Box::try_new(DevData {
            dev: Ref::try_new(device::Device::from_dev(pdev))?,
            _irq: None,
            irqnum: pdev.irq(),
            napi: napi.into(),
            tx_ring: None,
            rx_ring: None,

            re: Ref::try_new(re)?,
        })?;

        unsafe {
            bindings::dev_set_drvdata(
                &mut (*ndev.0).dev,
                Box::into_raw(devdata) as *const _ as *mut c_void,
            );
        }

        ndev.register()?;

        pr_info!(
            "(PCI:33MHz:32-bit) {:x}:{:x}:{:x}:{:x}:{:x}:{:x}",
            mac_addr[0],
            mac_addr[1],
            mac_addr[2],
            mac_addr[3],
            mac_addr[4],
            mac_addr[5]
        );
        pr_info!("Intel(R) PRO/1000 Network Connection");

        unsafe {
            bindings::netif_carrier_off(ndev.0);
        }

        Ok(Box::try_new(DrvData { _dev: ndev }).unwrap())
    }

    fn remove(_data: &Self::Data) {}

    kernel::define_pci_id_table! {(), [
        (pci::DeviceId::new(0x8086, DEV_ID_82540EM), None),
    ]}
}

struct E1000 {
    _driver: Pin<Box<driver::Registration<pci::Adapter<E1000>>>>,
}

impl kernel::Module for E1000 {
    fn init(name: &'static CStr, module: &'static ThisModule) -> Result<Self> {
        let _driver = driver::Registration::<pci::Adapter<E1000>>::new_pinned(name, module)?;
        Ok(E1000 { _driver })
    }
}

module! {
    type: E1000,
    name: b"rust_e1000",
    author: b"FUJITA Tomonori <fujita.tomonori@gmail.com>",
    description: b"Rust toy e1000 driver",
    license: b"GPL v2",
}
