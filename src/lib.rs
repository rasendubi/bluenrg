#![no_std]

extern crate ble;
extern crate embedded_hal as hal;
extern crate nb;

use core::marker::PhantomData;

mod cb;

pub struct BlueNRG<'buf, SPI, OutputPin, InputPin> {
    chip_select: OutputPin,
    data_ready: InputPin,
    rx_buffer: cb::Buffer<'buf, u8>,
    _spi: PhantomData<SPI>,
}

struct ActiveBlueNRG<'spi, 'dbuf: 'spi, SPI: 'spi, OutputPin: 'spi, InputPin: 'spi> {
    d: &'spi mut BlueNRG<'dbuf, SPI, OutputPin, InputPin>,
    spi: &'spi mut SPI,
}

#[derive(Copy, Clone, Debug)]
pub enum Error<E> {
    Comm(E),
    BLE(ble::hci::EventError),
}

fn parse_spi_header<E>(header: &[u8; 5]) -> Result<(u16, u16), nb::Error<Error<E>>> {
    const BNRG_READY: u8 = 0x02;
    if header[0] != BNRG_READY {
        Err(nb::Error::WouldBlock)
    } else {
        Ok((
            (header[2] as u16) << 8 | header[1] as u16,
            (header[4] as u16) << 8 | header[3] as u16,
        ))
    }
}

fn max<T: PartialOrd>(lhs: T, rhs: T) -> T {
    if lhs < rhs {
        rhs
    } else {
        lhs
    }
}

impl<'spi, 'dbuf, SPI, OutputPin, InputPin, E> ActiveBlueNRG<'spi, 'dbuf, SPI, OutputPin, InputPin>
where
    SPI: hal::blocking::spi::Transfer<u8, Error = E> + hal::blocking::spi::Write<u8, Error = E>,
    OutputPin: hal::digital::OutputPin,
    InputPin: hal::digital::InputPin,
{
    fn try_write(&mut self, header: &[u8], payload: &[u8]) -> nb::Result<(), Error<E>> {
        let mut write_header = [0x0a, 0x00, 0x00, 0x00, 0x00];
        self.spi
            .transfer(&mut write_header)
            .map_err(|e| nb::Error::Other(Error::Comm(e)))?;

        let (write_len, _read_len) = parse_spi_header(&write_header)?;
        if (write_len as usize) < header.len() + payload.len() {
            return Err(nb::Error::WouldBlock);
        }

        self.spi
            .write(header)
            .map_err(|e| nb::Error::Other(Error::Comm(e)))?;
        self.spi
            .write(payload)
            .map_err(|e| nb::Error::Other(Error::Comm(e)))?;

        Ok(())
    }

    fn try_read(&mut self) -> nb::Result<ble::Event, Error<E>> {
        // Always read whatever data is available, then get the next event from the internal buffer.
        // If there is no valid event, then use the return value from reading the data.  This
        // ensures that we can get a known pending event even if reading data would block.
        let data_result = self.read_available_data();
        match self.take_next_event() {
            Err(nb::Error::WouldBlock) => match data_result {
                Ok(_) => Err(nb::Error::WouldBlock),
                Err(e) => Err(e),
            },
            x => x,
        }
    }

    fn read_available_data(&mut self) -> nb::Result<(), Error<E>> {
        if !self.d.data_ready() {
            return Err(nb::Error::WouldBlock);
        }

        let mut read_header = [0x0b, 0x00, 0x00, 0x00, 0x00];
        self.spi
            .transfer(&mut read_header)
            .map_err(|e| nb::Error::Other(Error::Comm(e)))?;

        let (_write_len, read_len) = parse_spi_header(&read_header)?;
        let mut bytes_available = read_len as usize;
        while bytes_available > 0 && self.d.rx_buffer.next_contiguous_slice_len() > 0 {
            let transfer_count = max(
                bytes_available,
                self.d.rx_buffer.next_contiguous_slice_len(),
            );
            {
                let rx = self.d.rx_buffer.next_mut_slice(transfer_count);
                for i in 0..rx.len() {
                    rx[i] = 0;
                }
                self.spi
                    .transfer(rx)
                    .map_err(|e| nb::Error::Other(Error::Comm(e)))?;
            }
            bytes_available -= transfer_count;
        }

        Ok(())
    }

    fn take_next_event(&mut self) -> nb::Result<ble::Event, Error<E>> {
        if self.d.rx_buffer.available_len() < ble::hci::EVENT_PACKET_HEADER_LENGTH {
            return Err(nb::Error::WouldBlock);
        }

        let param_len = self.d.rx_buffer.peek(1) as usize;
        if self.d.rx_buffer.available_len() < ble::hci::EVENT_PACKET_HEADER_LENGTH + param_len {
            return Err(nb::Error::WouldBlock);
        }

        const MAX_EVENT_SIZE: usize = 128;
        let mut bytes: [u8; MAX_EVENT_SIZE] = [0; MAX_EVENT_SIZE];
        self.d
            .rx_buffer
            .take_slice(ble::hci::EVENT_PACKET_HEADER_LENGTH + param_len, &mut bytes);
        ble::hci::parse_event(ble::hci::EventPacket(&bytes))
            .map_err(|e| nb::Error::Other(Error::BLE(e)))
    }
}

impl<'spi, 'dbuf, SPI, OutputPin, InputPin, E> ble::Controller
    for ActiveBlueNRG<'spi, 'dbuf, SPI, OutputPin, InputPin>
where
    SPI: hal::blocking::spi::Transfer<u8, Error = E> + hal::blocking::spi::Write<u8, Error = E>,
    OutputPin: hal::digital::OutputPin,
    InputPin: hal::digital::InputPin,
{
    type Error = Error<E>;

    fn write(&mut self, header: &[u8], payload: &[u8]) -> nb::Result<(), Self::Error> {
        self.d.chip_select.set_low();
        let result = self.try_write(header, payload);
        self.d.chip_select.set_high();

        result
    }

    fn read(&mut self) -> nb::Result<ble::Event, Self::Error> {
        self.d.chip_select.set_low();
        let result = self.try_read();
        self.d.chip_select.set_high();

        result
    }
}

impl<'buf, SPI, OutputPin, InputPin> BlueNRG<'buf, SPI, OutputPin, InputPin>
where
    OutputPin: hal::digital::OutputPin,
    InputPin: hal::digital::InputPin,
{
    pub fn new<Reset>(
        rx_buffer: &'buf mut [u8],
        cs: OutputPin,
        dr: InputPin,
        reset: &mut Reset,
    ) -> BlueNRG<'buf, SPI, OutputPin, InputPin>
    where
        Reset: FnMut(),
    {
        reset();

        BlueNRG {
            chip_select: cs,
            rx_buffer: cb::Buffer::new(rx_buffer),
            data_ready: dr,
            _spi: PhantomData,
        }
    }

    pub fn with_spi<'spi, T, F, E>(&mut self, spi: &'spi mut SPI, body: F) -> T
    where
        F: FnOnce(&mut ble::Controller<Error = Error<E>>) -> T,
        SPI: hal::blocking::spi::transfer::Default<u8, Error = E>
            + hal::blocking::spi::write::Default<u8, Error = E>,
    {
        let mut active = ActiveBlueNRG::<SPI, OutputPin, InputPin> { spi: spi, d: self };
        body(&mut active as &mut ble::Controller<Error = Error<E>>)
    }

    fn data_ready(&self) -> bool {
        self.data_ready.is_high()
    }
}

pub struct Version {
    pub hw_version: u8,
    pub major: u8,
    pub minor: u8,
    pub patch: u8,
}

pub trait LocalVersionInfoExt {
    fn bluenrg_version(&self) -> Version;
}

impl LocalVersionInfoExt for ble::LocalVersionInfo {
    fn bluenrg_version(&self) -> Version {
        Version {
            hw_version: (self.hci_revision >> 8) as u8,
            major: (self.hci_revision & 0xFF) as u8,
            minor: ((self.lmp_subversion >> 4) & 0xF) as u8,
            patch: (self.lmp_subversion & 0xF) as u8,
        }
    }
}
