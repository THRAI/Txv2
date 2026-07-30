use crate::ondisk::Superblock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier1Request {
    DepthOneExtent,
    LinearDirectory,
    NonSplittingHtree,
    ClassicOrphan,
    ExtentDepthGrowth,
    HtreeSplit,
    OrphanFile,
    DirectIo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier1Reject {
    Unsupported,
    ProfileMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityProfileHash(pub [u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier1FeatureBits {
    pub compat: u32,
    pub incompat: u32,
    pub ro_compat: u32,
    pub metadata_csum: bool,
    pub ordered_jbd2: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier1Geometry {
    pub block_size: u32,
    pub inode_size: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier1MountFacts {
    pub feature_bits: Tier1FeatureBits,
    pub geometry: Tier1Geometry,
}

impl Tier1MountFacts {
    pub fn from_superblock(superblock: &Superblock) -> Self {
        Self {
            feature_bits: Tier1FeatureBits {
                compat: superblock.feature_compat,
                incompat: superblock.feature_incompat,
                ro_compat: superblock.feature_ro_compat,
                metadata_csum: superblock.has_metadata_csum(),
                // Ext4's internal journal is the only journal topology in the
                // pinned profile; the journal feature is ordered by admission.
                ordered_jbd2: superblock.journal_inode != 0,
            },
            geometry: Tier1Geometry {
                block_size: superblock.block_size(),
                inode_size: superblock.inode_size,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier1Capabilities {
    profile_hash: [u8; 32],
    feature_bits: Tier1FeatureBits,
    geometry: Tier1Geometry,
}

const GENERATED_TIER1_PROFILE: Tier1Capabilities = Tier1Capabilities {
    // Generated from tools/ext4/tier1/capability-ledger.json with its trailing
    // newline preserved. The host test hashes those exact authority bytes.
    profile_hash: [
        0xf5, 0x91, 0x34, 0x47, 0x93, 0xb6, 0x77, 0x42, 0xb6, 0x91, 0x29, 0x35, 0x9c, 0xfb, 0x9d,
        0x40, 0xa5, 0xce, 0x29, 0xea, 0x00, 0x8d, 0xb6, 0x0e, 0x4d, 0xa2, 0xf6, 0x03, 0xa4, 0x28,
        0xca, 0x8a,
    ],
    feature_bits: Tier1FeatureBits {
        compat: 0x0004,
        incompat: 0x20c0,
        ro_compat: 0x0408,
        metadata_csum: true,
        ordered_jbd2: true,
    },
    geometry: Tier1Geometry {
        block_size: 4096,
        inode_size: 256,
    },
};

impl Tier1Capabilities {
    pub const fn generated() -> Self {
        GENERATED_TIER1_PROFILE
    }

    pub const fn profile_hash(self) -> [u8; 32] {
        self.profile_hash
    }

    pub const fn admit(self, request: Tier1Request) -> Result<(), Tier1Reject> {
        match request {
            Tier1Request::DepthOneExtent
            | Tier1Request::LinearDirectory
            | Tier1Request::NonSplittingHtree
            | Tier1Request::ClassicOrphan => Ok(()),
            Tier1Request::ExtentDepthGrowth
            | Tier1Request::HtreeSplit
            | Tier1Request::OrphanFile
            | Tier1Request::DirectIo => Err(Tier1Reject::Unsupported),
        }
    }

    pub const fn admit_mount(
        self,
        facts: Tier1MountFacts,
    ) -> Result<CapabilityProfileHash, Tier1Reject> {
        let features = facts.feature_bits;
        let geometry = facts.geometry;
        if geometry.block_size != self.geometry.block_size
            || (geometry.inode_size != 128 && geometry.inode_size != self.geometry.inode_size)
            || features.compat & !self.feature_bits.compat != 0
            || features.incompat & !self.feature_bits.incompat != 0
            || features.incompat & Superblock::FEATURE_INCOMPAT_EXTENTS == 0
            || features.ro_compat & !self.feature_bits.ro_compat != 0
            || !features.metadata_csum
            || !features.ordered_jbd2
        {
            return Err(Tier1Reject::ProfileMismatch);
        }
        Ok(CapabilityProfileHash(self.profile_hash))
    }
}

/// Minimal no-std SHA-256 used by the host parity test. The generated profile
/// itself embeds the digest so kernel code performs no ledger I/O.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut state = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let mut blocks = bytes.chunks_exact(64);
    for block in &mut blocks {
        sha256_block(&mut state, block);
    }

    let remainder = blocks.remainder();
    let mut tail = [0u8; 128];
    tail[..remainder.len()].copy_from_slice(remainder);
    tail[remainder.len()] = 0x80;
    let tail_len = if remainder.len() < 56 { 64 } else { 128 };
    tail[tail_len - 8..tail_len].copy_from_slice(&((bytes.len() as u64) * 8).to_be_bytes());
    sha256_block(&mut state, &tail[..64]);
    if tail_len == 128 {
        sha256_block(&mut state, &tail[64..]);
    }

    let mut digest = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

fn sha256_block(state: &mut [u32; 8], block: &[u8]) {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];
    let mut words = [0u32; 64];
    for (index, word) in words[..16].iter_mut().enumerate() {
        *word = u32::from_be_bytes(block[index * 4..index * 4 + 4].try_into().unwrap());
    }
    for index in 16..64 {
        let s0 = words[index - 15].rotate_right(7)
            ^ words[index - 15].rotate_right(18)
            ^ (words[index - 15] >> 3);
        let s1 = words[index - 2].rotate_right(17)
            ^ words[index - 2].rotate_right(19)
            ^ (words[index - 2] >> 10);
        words[index] = words[index - 16]
            .wrapping_add(s0)
            .wrapping_add(words[index - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for (index, word) in words.iter().enumerate() {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choice = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(choice)
            .wrapping_add(K[index])
            .wrapping_add(*word);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}
