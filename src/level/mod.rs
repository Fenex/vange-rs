use byteorder::{LittleEndian as E, ReadBytesExt};
use std::io::{BufReader, Read, Seek, SeekFrom};

mod config;

pub use self::config::{LevelConfig, TerrainConfig};

pub type TerrainType = u8;
pub const NUM_TERRAINS: usize = 8;

pub type Altitude = u8;
pub type Delta = Altitude;
const DOUBLE_LEVEL: u8 = 1 << 6;
const DELTA_SHIFT0: u8 = 2 + 3;
const DELTA_SHIFT1: u8 = 0 + 3;
const DELTA_MASK: u8 = 0x3;
const TERRAIN_SHIFT: u8 = 3;
pub const HEIGHT_SCALE: u32 = 255;

pub struct Level {
    pub size: (i32, i32),
    pub flood_map: Vec<u32>,
    pub height: Vec<u8>,
    pub meta: Vec<u8>,
    pub palette: [[u8; 4]; 0x100],
    pub terrains: [TerrainConfig; NUM_TERRAINS],
}

pub struct Point(pub Altitude, pub TerrainType);

pub enum Texel {
    Single(Point),
    Dual {
        low: Point,
        high: Point,
        delta: Delta,
    }
}

impl Texel {
    pub fn top(&self) -> Altitude {
        match *self {
            Texel::Single(ref p) => p.0,
            Texel::Dual { ref high, .. } => high.0,
        }
    }
}

impl Level {
    pub fn new_test() -> Self {
        let tc = TerrainConfig {
            shadow_offset: 0,
            height_shift: 0,
            colors: 0..1,
        };
        Level {
            size: (2, 1),
            flood_map: vec![0],
            height: vec![0, 0],
            meta: vec![0, 0],
            palette: [[0xFF; 4]; 0x100],
            // not pretty, I know
            terrains: [
                tc.clone(), tc.clone(), tc.clone(), tc.clone(),
                tc.clone(), tc.clone(), tc.clone(), tc.clone(),
            ],
        }
    }

    pub fn get(
        &self,
        mut coord: (i32, i32),
    ) -> Texel {
        fn get_terrain(meta: u8) -> TerrainType {
            (meta >> TERRAIN_SHIFT) & (NUM_TERRAINS as u8 - 1)
        }
        while coord.0 < 0 {
            coord.0 += self.size.0;
        }
        while coord.1 < 0 {
            coord.1 += self.size.1;
        }
        let i = ((coord.1 % self.size.1) * self.size.0 + (coord.0 % self.size.0)) as usize;
        let meta = self.meta[i];
        if meta & DOUBLE_LEVEL != 0 {
            let meta0 = self.meta[i & !1];
            let meta1 = self.meta[i | 1];
            let d0 = (meta0 & DELTA_MASK) << DELTA_SHIFT0;
            let d1 = (meta1 & DELTA_MASK) << DELTA_SHIFT1;
            Texel::Dual {
                low: Point(self.height[i & !1], get_terrain(meta0)),
                high: Point(self.height[i | 1], get_terrain(meta1)),
                delta: d0 + d1,
            }
        } else {
            Texel::Single(Point(self.height[i], get_terrain(meta)))
        }
    }

    pub fn export(&self) -> Vec<u8> {
        let mut data = vec![0; self.size.0 as usize * self.size.1 as usize * 4];
        for y in 0 .. self.size.1 {
            let base_y = (y * self.size.0) as usize * 4;
            for x in 0 .. self.size.0 {
                let base_x = base_y + x as usize * 4;
                let mut color = &mut data[base_x .. base_x + 4];
                match self.get((x, y)) {
                    Texel::Single(Point(alt, ty)) => {
                        color[0] = alt;
                        color[1] = alt;
                        color[2] = 0;
                        color[3] = ty << 4;
                    }
                    Texel::Dual {
                        low: Point(low_alt, low_ty),
                        high: Point(high_alt, high_ty),
                        delta,
                    } => {
                        color[0] = low_alt;
                        color[1] = high_alt;
                        color[2] = delta;
                        color[3] = low_ty + (high_ty << 4);
                    }
                }
            }
        }
        data
    }
}

#[allow(unused)]
fn print_palette(data: &[[u8; 4]], info: &str) {
    print!("Palette - {}:", info);
    for i in 0 .. 3 {
        print!("\n\t");
        for j in 0 .. 0x100 {
            print!("{:02X}", data[j][i]);
        }
    }
    print!("\n");
}

pub fn read_palette<I: Read>(input: I, config: Option<&[TerrainConfig]>) -> [[u8; 4]; 0x100] {
    let mut file = BufReader::new(input);
    let mut data = [[0; 4]; 0x100];
    for p in data.iter_mut() {
        file.read(&mut p[.. 3]).unwrap();
        //p[0] <<= 2; p[1] <<= 2; p[2] <<= 2;
    }
    //print_palette(&data, "read from file");
    if let Some(terrains) = config {
        // see `PalettePrepare` of the original
        data[0] = [0; 4];

        for tc in terrains {
            for c in &mut data[tc.colors.start as usize][.. 3] {
                *c >>= 1;
            }
        }

        for i in 0 .. 16 {
            let mut value = [(i * 4) as u8; 4];
            value[3] = 0;
            data[224 + i] = value;
        }

        //print_palette(&data, "corrected");
    }
    // see `XGR_Screen::setpal` of the original
    for p in data.iter_mut() {
        p[0] <<= 2; p[1] <<= 2; p[2] <<= 2;
    }
    //print_palette(&data, "scale");
    //TODO: there is quite a bit of logic missing here,
    // see `GeneralTableOpen` and `PalettePrepare` of the original.
    data
}

pub fn load(config: &LevelConfig) -> Level {
    use rayon::prelude::*;
    use splay::Splay;
    use std::fs::File;
    use std::time::Instant;

    fn report_time(start: Instant) {
        let d = Instant::now() - start;
        info!(
            "\ttook {} ms",
            d.as_secs() as u32 * 1000 + d.subsec_nanos() / 1_000_000
        );
    }

    assert!(config.is_compressed);
    let size = (config.size.0.as_value(), config.size.1.as_value());

    info!("Loading vpr...");
    let start_vpr = Instant::now();
    let flood_map = {
        let vpr_file = File::open(&config.path_vpr).unwrap();
        let flood_size = size.1 >> config.section.as_power();
        let geo_pow = config.geo.as_power();
        let net_size = size.0 * size.1 >> (2 * geo_pow);
        let flood_offset = (2 * 4 + (1 + 4 + 4) * 4 + 2 * net_size + 2 * geo_pow * 4
            + 2 * flood_size * geo_pow * 4) as u64;
        let expected_file_size = flood_offset + (flood_size * 4) as u64;
        assert_eq!(
            vpr_file.metadata().unwrap().len(),
            expected_file_size as u64
        );
        let mut vpr = BufReader::new(vpr_file);
        vpr.seek(SeekFrom::Start(flood_offset)).unwrap();
        (0 .. flood_size)
            .map(|_| vpr.read_u32::<E>().unwrap())
            .collect()
    };
    report_time(start_vpr);

    info!("Loading vmc...");
    let start_vmc = Instant::now();
    let (height, meta) = {
        let mut vmc_base = BufReader::new(File::open(&config.path_vmc).unwrap());
        info!("\tLoading compression tables...");
        let mut st_table = Vec::<i32>::with_capacity(size.1 as usize);
        let mut sz_table = Vec::<i16>::with_capacity(size.1 as usize);
        for _ in 0 .. size.1 {
            st_table.push(vmc_base.read_i32::<E>().unwrap());
            sz_table.push(vmc_base.read_i16::<E>().unwrap());
        }
        info!("\tDecompressing level data...");
        let splay = Splay::new(&mut vmc_base);
        let total = (size.0 * size.1) as usize;
        let mut height = vec![0u8; total];
        let mut meta = vec![0u8; total];

        height
            .chunks_mut(size.0 as _)
            .zip(meta.chunks_mut(size.0 as _))
            .zip(st_table.iter())
            .collect::<Vec<_>>()
            .par_chunks_mut(64)
            .for_each(|source_group| {
                //Note: a separate file per group is required
                let mut vmc = BufReader::new(File::open(&config.path_vmc).unwrap());
                for &mut ((ref mut h_row, ref mut m_row), offset) in source_group {
                    vmc.seek(SeekFrom::Start(*offset as u64)).unwrap();
                    splay.expand1(&mut vmc, h_row);
                    splay.expand2(&mut vmc, m_row);
                }
            });

        (height, meta)
    };
    report_time(start_vmc);
    let palette = File::open(&config.path_palette)
        .expect("Unable to open the palette file");

    Level {
        size,
        flood_map,
        height,
        meta,
        palette: read_palette(palette, Some(&config.terrains)),
        terrains: config.terrains.clone(),
    }
}
