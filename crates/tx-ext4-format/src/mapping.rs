//! Pure extent-root mapping; callers fetch `NeedNode` metadata asynchronously.

use crate::ondisk::{BlockMapping, ExtentNode};
use crate::Result;

pub fn map_extent_root(root: &[u8], logical_block: u32) -> Result<BlockMapping> {
    match ExtentNode::parse(root)? {
        ExtentNode::Leaf(extents) => Ok(extents
            .into_iter()
            .find_map(|extent| extent.physical_for(logical_block))
            .map(BlockMapping::Data)
            .unwrap_or(BlockMapping::Hole)),
        ExtentNode::Index(indexes) => Ok(indexes
            .iter()
            .copied()
            .take_while(|index| index.logical_block <= logical_block)
            .last()
            .or_else(|| indexes.first().copied())
            .map(|index| BlockMapping::NeedNode(index.child))
            .unwrap_or(BlockMapping::Hole)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_root_maps_logical_block_without_io() {
        let mut root = [0u8; 60];
        root[0..2].copy_from_slice(&0xf30au16.to_le_bytes());
        root[2..4].copy_from_slice(&1u16.to_le_bytes());
        root[4..6].copy_from_slice(&4u16.to_le_bytes());
        root[12..16].copy_from_slice(&3u32.to_le_bytes());
        root[16..18].copy_from_slice(&2u16.to_le_bytes());
        root[20..24].copy_from_slice(&100u32.to_le_bytes());

        assert_eq!(map_extent_root(&root, 4), Ok(BlockMapping::Data(101)));
        assert_eq!(map_extent_root(&root, 5), Ok(BlockMapping::Hole));
    }
}
