//! The page draws every block as one flat colour of its material. The materials are ours: a
//! block's material follows from its registry name alone, and the colours are the page's
//! (fleet/web/core/minecraft.ts, `MATERIAL_COLORS`). No texture, colour table or other asset of
//! the game is used.

/// The materials, by index; the page's colour table has one entry for each, in this order.
pub const MATERIALS: [&str; 48] = [
    "unknown",
    "water",
    "grass",
    "dirt",
    "sand",
    "stone",
    "gravel",
    "snow",
    "ice",
    "wood",
    "leaves",
    "lava",
    "clay",
    "plant",
    "planks",
    "brick",
    "glass",
    "path",
    "mud",
    "deepslate",
    "netherrack",
    "red_sand",
    "flower",
    "crop",
    "metal",
    "glow",
    "obsidian",
    "terracotta",
    "moss",
    "mushroom",
    "farmland",
    "cobblestone",
    "white",
    "orange",
    "magenta",
    "light_blue",
    "yellow",
    "lime",
    "pink",
    "gray",
    "light_gray",
    "cyan",
    "purple",
    "blue",
    "brown",
    "green",
    "red",
    "black",
];

/// The sixteen dye colours, in the order of `MATERIALS[32..]`.
const DYES: [&str; 16] = [
    "white",
    "orange",
    "magenta",
    "light_blue",
    "yellow",
    "lime",
    "pink",
    "gray",
    "light_gray",
    "cyan",
    "purple",
    "blue",
    "brown",
    "green",
    "red",
    "black",
];

pub fn index(name: &str) -> u8 {
    MATERIALS.iter().position(|m| *m == name).unwrap_or(0) as u8
}

/// The material of the block named `name` (its registry name, with or without the namespace).
pub fn material(name: &str) -> u8 {
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    // Dyed blocks (wool, concrete, terracotta, glass, carpet, beds) are their dye's colour.
    if name.ends_with("_wool")
        || name.ends_with("_concrete")
        || name.ends_with("_carpet")
        || name.ends_with("_concrete_powder")
        || name.ends_with("_stained_glass")
        || name.ends_with("_terracotta") && !name.contains("glazed")
    {
        // light_blue and light_gray before blue and gray.
        let mut best: Option<(usize, usize)> = None;
        for (i, d) in DYES.iter().enumerate() {
            if name.starts_with(&format!("{d}_")) && best.is_none_or(|(_, l)| d.len() > l) {
                best = Some((i, d.len()));
            }
        }
        if let Some((i, _)) = best {
            return 32 + i as u8;
        }
    }
    let m = match name {
        "water" | "bubble_column" | "kelp" | "kelp_plant" | "seagrass" | "tall_seagrass" => "water",
        "lava" | "magma_block" => "lava",
        "grass_block" => "grass",
        "dirt" | "coarse_dirt" | "rooted_dirt" => "dirt",
        "dirt_path" => "path",
        "farmland" => "farmland",
        "podzol" | "mycelium" => "mushroom",
        "sand" | "sandstone" | "smooth_sandstone" | "cut_sandstone" | "suspicious_sand" => "sand",
        "red_sand" | "red_sandstone" => "red_sand",
        "gravel" | "suspicious_gravel" => "gravel",
        "snow" | "snow_block" | "powder_snow" => "snow",
        "ice" | "packed_ice" | "blue_ice" | "frosted_ice" => "ice",
        "clay" => "clay",
        "terracotta" => "terracotta",
        "mud" | "packed_mud" | "mud_bricks" => "mud",
        "moss_block" | "moss_carpet" => "moss",
        "obsidian" | "crying_obsidian" => "obsidian",
        "netherrack" => "netherrack",
        "glass" | "glass_pane" | "tinted_glass" => "glass",
        "glowstone" | "sea_lantern" | "shroomlight" | "torch" | "lantern" => "glow",
        "cobblestone" | "mossy_cobblestone" => "cobblestone",
        _ => "",
    };
    if !m.is_empty() {
        return index(m);
    }
    let has = |s: &str| name.contains(s);
    let m = if has("deepslate") || has("tuff") || has("basalt") || has("blackstone") {
        "deepslate"
    } else if has("leaves") || has("leaf_litter") || has("vine") || has("azalea") {
        "leaves"
    } else if has("planks")
        || has("_stairs") && has("oak")
        || has("fence")
        || has("door")
        || has("bookshelf")
    {
        "planks"
    } else if has("_log") || has("_wood") || has("_stem") || has("hyphae") || has("bamboo") {
        "wood"
    } else if has("brick") {
        "brick"
    } else if has("tulip")
        || has("poppy")
        || has("dandelion")
        || has("orchid")
        || has("allium")
        || has("bluet")
        || has("daisy")
        || has("cornflower")
        || has("lily")
        || has("rose")
        || has("peony")
        || has("lilac")
        || has("sunflower")
        || has("flower")
    {
        "flower"
    } else if has("wheat")
        || has("carrots")
        || has("potatoes")
        || has("beetroots")
        || has("melon")
        || has("pumpkin")
    {
        "crop"
    } else if has("grass")
        || has("fern")
        || has("bush")
        || has("sapling")
        || has("cactus")
        || has("sugar_cane")
        || has("dead_bush")
        || has("mushroom")
        || has("lily_pad")
        || has("dripleaf")
    {
        "plant"
    } else if has("_ore") {
        "stone"
    } else if has("iron") || has("gold") || has("copper") || has("anvil") || has("rail") {
        "metal"
    } else if has("stone")
        || has("granite")
        || has("diorite")
        || has("andesite")
        || has("calcite")
        || has("dripstone")
        || has("bedrock")
    {
        "stone"
    } else {
        "unknown"
    };
    index(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_surface_blocks_have_their_own_material() {
        for (name, want) in [
            ("grass_block", "grass"),
            ("water", "water"),
            ("sand", "sand"),
            ("stone", "stone"),
            ("oak_log", "wood"),
            ("birch_leaves", "leaves"),
            ("snow", "snow"),
            ("gravel", "gravel"),
            ("oak_planks", "planks"),
            ("short_grass", "plant"),
            ("poppy", "flower"),
            ("deepslate", "deepslate"),
            ("iron_ore", "stone"),
            ("lava", "lava"),
            ("dirt_path", "path"),
            ("cobblestone", "cobblestone"),
            ("seagrass", "water"),
            ("minecraft:grass_block", "grass"),
            ("minecraft:red_wool", "red"),
        ] {
            assert_eq!(MATERIALS[material(name) as usize], want, "{name}");
        }
    }

    #[test]
    fn dyed_blocks_take_their_dye_with_light_colours_before_plain_ones() {
        assert_eq!(MATERIALS[material("red_wool") as usize], "red");
        assert_eq!(
            MATERIALS[material("light_blue_wool") as usize],
            "light_blue"
        );
        assert_eq!(
            MATERIALS[material("light_gray_concrete") as usize],
            "light_gray"
        );
        assert_eq!(MATERIALS[material("blue_concrete") as usize], "blue");
        assert_eq!(MATERIALS[material("black_terracotta") as usize], "black");
        assert_eq!(MATERIALS[material("terracotta") as usize], "terracotta");
    }

    #[test]
    fn an_unknown_name_is_unknown() {
        assert_eq!(material("something_new"), 0);
        assert_eq!(MATERIALS.len(), 48);
        assert_eq!(&MATERIALS[32..], &DYES);
    }
}
