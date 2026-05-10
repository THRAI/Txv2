#![cfg_attr(not(any(test, feature = "std")), no_std)]

extern crate alloc;

pub mod virtio;

pub mod spi {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum SpiError {
        Transport,
        Protocol,
        Unsupported,
    }

    pub trait SpiBus {
        fn transfer(
            &mut self,
            chip_select: u8,
            write: &[u8],
            read: &mut [u8],
        ) -> Result<(), SpiError>;
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct SpiDevice {
        pub bus: u8,
        pub chip_select: u8,
        pub mode: u8,
        pub max_hz: u32,
    }

    impl SpiDevice {
        pub const fn new(bus: u8, chip_select: u8, mode: u8, max_hz: u32) -> Self {
            Self {
                bus,
                chip_select,
                mode,
                max_hz,
            }
        }
    }
}

pub mod sd_spi {
    use super::spi::{SpiBus, SpiDevice, SpiError};

    pub const BLOCK_SIZE: usize = 512;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum SdCommand {
        GoIdle,
        SendIfCond,
        AppCmd,
        SdSendOpCond,
        SetBlockLen,
        ReadSingleBlock,
        WriteSingleBlock,
    }

    impl SdCommand {
        pub const fn index(self) -> u8 {
            match self {
                Self::GoIdle => 0,
                Self::SendIfCond => 8,
                Self::AppCmd => 55,
                Self::SdSendOpCond => 41,
                Self::SetBlockLen => 16,
                Self::ReadSingleBlock => 17,
                Self::WriteSingleBlock => 24,
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum SdSpiError {
        Spi(SpiError),
        BadResponse,
    }

    pub struct SpiSdCard<B> {
        bus: B,
        device: SpiDevice,
    }

    impl<B: SpiBus> SpiSdCard<B> {
        pub const fn new(bus: B, device: SpiDevice) -> Self {
            Self { bus, device }
        }

        pub fn init(&mut self) -> Result<(), SdSpiError> {
            self.command(SdCommand::GoIdle, 0)?;
            self.command(SdCommand::SendIfCond, 0x1aa)?;
            self.command(SdCommand::AppCmd, 0)?;
            self.command(SdCommand::SdSendOpCond, 0x4000_0000)?;
            self.command(SdCommand::SetBlockLen, BLOCK_SIZE as u32)?;
            Ok(())
        }

        pub fn read_block(
            &mut self,
            lba: u32,
            out: &mut [u8; BLOCK_SIZE],
        ) -> Result<(), SdSpiError> {
            self.command(SdCommand::ReadSingleBlock, lba)?;
            let write = [0xff; BLOCK_SIZE];
            self.bus
                .transfer(self.device.chip_select, &write, out)
                .map_err(SdSpiError::Spi)?;
            Ok(())
        }

        pub fn write_block(&mut self, lba: u32, data: &[u8; BLOCK_SIZE]) -> Result<(), SdSpiError> {
            self.command(SdCommand::WriteSingleBlock, lba)?;
            let mut sink = [0u8; BLOCK_SIZE];
            self.bus
                .transfer(self.device.chip_select, data, &mut sink)
                .map_err(SdSpiError::Spi)?;
            Ok(())
        }

        fn command(&mut self, command: SdCommand, argument: u32) -> Result<(), SdSpiError> {
            let frame = sd_command_frame(command, argument);
            let mut response = [0xff; 1];
            self.bus
                .transfer(self.device.chip_select, &frame, &mut response)
                .map_err(SdSpiError::Spi)?;
            if response[0] == 0xff {
                return Err(SdSpiError::BadResponse);
            }
            Ok(())
        }
    }

    pub const fn sd_command_frame(command: SdCommand, argument: u32) -> [u8; 6] {
        let index = 0x40 | command.index();
        let arg = argument.to_be_bytes();
        let crc = match command {
            SdCommand::GoIdle => 0x95,
            SdCommand::SendIfCond => 0x87,
            _ => 0x01,
        };
        [index, arg[0], arg[1], arg[2], arg[3], crc]
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        struct ScriptedSpi {
            responses: &'static [u8],
            next: usize,
        }

        impl SpiBus for ScriptedSpi {
            fn transfer(
                &mut self,
                _chip_select: u8,
                write: &[u8],
                read: &mut [u8],
            ) -> Result<(), SpiError> {
                if write.len() == 6 {
                    read[0] = self.responses[self.next];
                    self.next += 1;
                } else {
                    for (idx, byte) in read.iter_mut().enumerate() {
                        *byte = idx as u8;
                    }
                }
                Ok(())
            }
        }

        #[test]
        fn builds_standard_sd_command_frames() {
            assert_eq!(
                sd_command_frame(SdCommand::GoIdle, 0),
                [0x40, 0, 0, 0, 0, 0x95]
            );
            assert_eq!(
                sd_command_frame(SdCommand::SendIfCond, 0x1aa),
                [0x48, 0, 0, 0x01, 0xaa, 0x87]
            );
        }

        #[test]
        fn scripted_spi_sd_init_and_read_work() {
            let spi = ScriptedSpi {
                responses: &[0x01, 0x01, 0x01, 0x00, 0x00, 0x00],
                next: 0,
            };
            let mut card = SpiSdCard::new(spi, SpiDevice::new(0, 0, 0, 25_000_000));
            card.init().unwrap();

            let mut block = [0u8; BLOCK_SIZE];
            card.read_block(0, &mut block).unwrap();
            assert_eq!(block[0], 0);
            assert_eq!(block[1], 1);
        }
    }
}
