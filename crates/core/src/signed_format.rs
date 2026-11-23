//! The signed program formats the solidity-sdk releases accept.

use alloy_primitives::FixedBytes;

/// The signed format a consumer's solidity-sdk release accepts. A program or tree signed in one
/// format doesn't settle on a consumer built from the other release.
///
/// Each release reports its own `IGasKillerSDK` interface ID through ERC-165, which is how a
/// caller tells them apart ([`SignedFormat::from_interface_id`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignedFormat {
    /// solidity-sdk v0.1.0. The digest, and the nested root leaf with it, doesn't bind the
    /// chain id.
    V0_1,
    /// solidity-sdk v0.2.0. The digest and the nested root leaf bind the chain id after the
    /// transition index.
    V0_2,
}

impl SignedFormat {
    /// The `IGasKillerSDK` interface ID a consumer of this release reports.
    pub const fn interface_id(self) -> FixedBytes<4> {
        match self {
            Self::V0_1 => FixedBytes([0x82, 0xb3, 0x5a, 0x01]),
            Self::V0_2 => FixedBytes([0x57, 0xa9, 0xb0, 0x02]),
        }
    }

    /// The format of a consumer reporting `id`, or `None` for an unknown ID.
    ///
    /// Unreleased solidity-sdk builds between v0.1.0 and v0.2.0 report the v0.1.0 ID but already
    /// bind the chain id. They map to [`SignedFormat::V0_1`] here and won't settle its trees.
    pub fn from_interface_id(id: FixedBytes<4>) -> Option<Self> {
        [Self::V0_1, Self::V0_2]
            .into_iter()
            .find(|format| format.interface_id() == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_ids_round_trip() {
        for format in [SignedFormat::V0_1, SignedFormat::V0_2] {
            assert_eq!(
                SignedFormat::from_interface_id(format.interface_id()),
                Some(format)
            );
        }
        assert_eq!(SignedFormat::from_interface_id(FixedBytes::ZERO), None);
    }
}
