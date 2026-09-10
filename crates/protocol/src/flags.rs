//! Synology private Btrfs root-flag policy (SPEC §6).
//!
//! Upstream `btrfs-progs` rejects any root flag outside
//! `BTRFS_ROOT_SUBVOL_RDONLY | BTRFS_ROOT_SUBVOL_DEAD`. Synology sets private
//! bits, so extraction stops with `invalid root flags, have 0x...`.
//!
//! The fix is to widen the mask by exactly the bits we have reproduced on our
//! own fixture — never to disable the check. Public patches that comment out
//! the whole comparison also let genuinely corrupt roots through, which is why
//! this module models the decision explicitly instead.

use serde::{Deserialize, Serialize};

/// `BTRFS_ROOT_SUBVOL_RDONLY` (bit 0).
pub const BTRFS_ROOT_SUBVOL_RDONLY: u64 = 1 << 0;
/// `BTRFS_ROOT_SUBVOL_DEAD` (bit 48).
pub const BTRFS_ROOT_SUBVOL_DEAD: u64 = 1 << 48;

/// The upstream mask, reproduced so tests can assert we only ever widen it.
pub const UPSTREAM_VALID_ROOT_FLAGS: u64 = BTRFS_ROOT_SUBVOL_RDONLY | BTRFS_ROOT_SUBVOL_DEAD;

/// Synology bit 34 (`0x400000000`) — the only bit reproduced on our own disk.
/// Reported elsewhere as marking `@sharesnap`, `@img_bkp_cache`, `@synologydrive`.
pub const SYNO_BIT_34: u64 = 1 << 34;

/// Bits observed or reported but **not** reproduced by us. Never allowed in a
/// production build; listed so diagnostics can name them precisely.
pub const SYNO_BIT_32: u64 = 1 << 32;
pub const SYNO_BIT_33: u64 = 1 << 33;
pub const SYNO_BIT_35: u64 = 1 << 35;

/// How much we actually know about a flag bit (SPEC §6 evidence tiers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// Reproduced on our own disk image. Only this tier reaches production.
    OwnFixture,
    /// Credible third-party report; we have no fixture.
    ExternalReport,
    /// Bit position mentioned somewhere; unverified.
    Candidate,
}

/// Which mask to apply. A research build may accept reported bits so we can
/// build fixtures for them; it must never ship to users.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    #[default]
    Production,
    Research,
}

/// Known Synology bits and their evidence tier.
/// Bits 33, 34 and 35 were all observed together on a real Synology DSM
/// volume during testing: `0x400000000` on ordinary shared folders,
/// `0x600000000` on `Snapshot`, and `0xc00000000` on UUID-named
/// subvolumes. Bit 32 remains third-party report only.
pub const KNOWN_SYNOLOGY_FLAGS: &[(u64, Evidence)] = &[
    (SYNO_BIT_34, Evidence::OwnFixture),
    (SYNO_BIT_33, Evidence::OwnFixture),
    (SYNO_BIT_35, Evidence::OwnFixture),
    (SYNO_BIT_32, Evidence::ExternalReport),
];

/// The accepted mask for `profile`.
///
/// Production widens upstream by the three bits our own disk carries.
pub fn valid_root_flags(profile: Profile) -> u64 {
    let mut mask = UPSTREAM_VALID_ROOT_FLAGS | SYNO_BIT_33 | SYNO_BIT_34 | SYNO_BIT_35;
    if profile == Profile::Research {
        mask |= SYNO_BIT_32;
    }
    mask
}

/// Outcome of checking one root item's flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagDecision {
    /// Every bit is inside the accepted mask.
    Accept,
    /// At least one bit is outside it. Stop this root and export diagnostics;
    /// the source is not modified either way.
    Reject {
        /// The bits that were not accepted, for the diagnostic bundle.
        unknown_bits: u64,
        /// Evidence tier if we recognise the bits, `None` if wholly unknown.
        evidence: Option<Evidence>,
    },
}

/// Decide whether a root item's flags may be extracted.
pub fn check_root_flags(flags: u64, profile: Profile) -> FlagDecision {
    let unknown_bits = flags & !valid_root_flags(profile);
    if unknown_bits == 0 {
        return FlagDecision::Accept;
    }
    // Report the weakest evidence among the offending bits: if any bit is
    // wholly unrecognised, that is what the operator needs to know about.
    let evidence = KNOWN_SYNOLOGY_FLAGS
        .iter()
        .filter(|(bit, _)| unknown_bits & bit != 0)
        .map(|(_, ev)| *ev)
        .max_by_key(|ev| match ev {
            Evidence::OwnFixture => 0,
            Evidence::ExternalReport => 1,
            Evidence::Candidate => 2,
        })
        .filter(|_| unknown_bits & !known_bits_mask() == 0);
    FlagDecision::Reject { unknown_bits, evidence }
}

fn known_bits_mask() -> u64 {
    KNOWN_SYNOLOGY_FLAGS.iter().fold(0, |acc, (bit, _)| acc | bit)
}

/// Render the error the way upstream does, for logs and diagnostics.
pub fn describe_rejection(flags: u64, profile: Profile) -> String {
    format!(
        "invalid root flags, have 0x{:x} expect mask 0x{:x}",
        flags,
        valid_root_flags(profile)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_mask_matches_btrfs_progs() {
        // btrfs-progs kernel-shared/tree-checker.c prints this mask.
        assert_eq!(UPSTREAM_VALID_ROOT_FLAGS, 0x1000000000001);
    }

    #[test]
    fn production_widens_only_by_fixture_proven_bits() {
        let widened = valid_root_flags(Profile::Production) & !UPSTREAM_VALID_ROOT_FLAGS;
        assert_eq!(widened, SYNO_BIT_33 | SYNO_BIT_34 | SYNO_BIT_35);
        assert_eq!(widened & SYNO_BIT_32, 0, "bit 32 has no fixture of ours");
    }

    /// The exact values read off our own Synology disk.
    #[test]
    fn every_flag_combination_seen_on_the_real_disk_is_accepted() {
        for (value, what) in [
            (0x400000000u64, "shared folders: web, office, homes"),
            (0x600000000, "Snapshot"),
            (0xc00000000, "UUID-named subvolumes"),
            (0x0, "ordinary subvolumes such as @syno and Picture"),
        ] {
            assert_eq!(
                check_root_flags(value, Profile::Production),
                FlagDecision::Accept,
                "0x{value:x} ({what}) was read from a real disk and must be accepted"
            );
        }
    }

    #[test]
    fn the_flag_from_our_own_disk_is_accepted() {
        // The value in the Phase 0 recovery: 0x400000000.
        assert_eq!(SYNO_BIT_34, 0x400000000);
        let flags = BTRFS_ROOT_SUBVOL_RDONLY | SYNO_BIT_34;
        assert_eq!(check_root_flags(flags, Profile::Production), FlagDecision::Accept);
    }

    #[test]
    fn a_bit_with_no_fixture_of_ours_still_fails_safe() {
        // Bit 32 appears only in third-party reports, so production refuses it
        // even though its neighbours are now proven.
        match check_root_flags(SYNO_BIT_32, Profile::Production) {
            FlagDecision::Reject { unknown_bits, evidence } => {
                assert_eq!(unknown_bits, SYNO_BIT_32);
                assert_eq!(evidence, Some(Evidence::ExternalReport));
            }
            FlagDecision::Accept => panic!("bit 32 must not be accepted without a fixture"),
        }
    }

    #[test]
    fn research_profile_accepts_reported_bits_so_fixtures_can_be_built() {
        assert_eq!(check_root_flags(SYNO_BIT_32, Profile::Research), FlagDecision::Accept);
    }

    #[test]
    fn a_wholly_unknown_bit_is_reported_without_evidence() {
        let rogue = 1 << 40;
        match check_root_flags(rogue, Profile::Research) {
            FlagDecision::Reject { unknown_bits, evidence } => {
                assert_eq!(unknown_bits, rogue);
                assert_eq!(evidence, None, "an unrecognised bit must not borrow evidence");
            }
            FlagDecision::Accept => panic!("unknown bit must never be accepted"),
        }
    }

    #[test]
    fn corruption_is_still_caught_after_widening() {
        // The point of an allow-list over commenting out the check: a root
        // with real garbage in it must still be rejected in production.
        let corrupt = 0xdead_0000_0000_0000;
        assert!(matches!(
            check_root_flags(corrupt, Profile::Production),
            FlagDecision::Reject { .. }
        ));
    }

    #[test]
    fn rejection_message_matches_upstream_wording() {
        let msg = describe_rejection(SYNO_BIT_32, Profile::Production);
        // 0x1000e00000001 = DEAD(bit48) | bits 35,34,33 | RDONLY(bit0).
        assert_eq!(
            msg,
            "invalid root flags, have 0x100000000 expect mask 0x1000e00000001"
        );
    }
}
