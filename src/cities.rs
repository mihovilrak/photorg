//! The embedded GeoNames `cities1000` dataset and its k-d tree.
//!
//! The upstream dump is a 7.8 MB CSV, which on its own breaks the binary-size
//! NFR. `data/make_cities.py` compiles it into a packed blob — unused columns
//! dropped, region names interned, coordinates narrowed to `f32` — and
//! writes the records in k-d tree order, so lookups are pointer-free index
//! arithmetic over `include_bytes!` and startup costs nothing.

const DATA: &[u8] = include_bytes!("../data/cities.bin");
const MAGIC: &[u8; 4] = b"PGC1";
const HEADER: usize = 20;
const RECORD: usize = 17;

/// The nearest populated place to a coordinate.
pub struct Place {
    pub name: &'static str,
    pub region: &'static str,
    pub cc: &'static str,
}

struct Dataset {
    records: &'static [u8],
    offsets: &'static [u8],
    regions: &'static str,
    names: &'static str,
    count: usize,
}

fn dataset() -> &'static Dataset {
    static LOADED: std::sync::OnceLock<Dataset> = std::sync::OnceLock::new();
    LOADED.get_or_init(|| Dataset::load(DATA).expect("the embedded dataset is malformed"))
}

impl Dataset {
    fn load(data: &'static [u8]) -> Option<Dataset> {
        if data.len() < HEADER || &data[..4] != MAGIC {
            return None;
        }
        let word = |i: usize| u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        let (count, regions, names_len, regions_len) = (word(4), word(8), word(12), word(16));

        let records_end = HEADER.checked_add(count.checked_mul(RECORD)?)?;
        let offsets_end = records_end.checked_add((regions + 1).checked_mul(4)?)?;
        let regions_end = offsets_end.checked_add(regions_len)?;
        let names_end = regions_end.checked_add(names_len)?;
        if names_end != data.len() {
            return None;
        }

        Some(Dataset {
            records: &data[HEADER..records_end],
            offsets: &data[records_end..offsets_end],
            regions: std::str::from_utf8(&data[offsets_end..regions_end]).ok()?,
            names: std::str::from_utf8(&data[regions_end..names_end]).ok()?,
            count,
        })
    }

    fn coords(&self, i: usize) -> (f32, f32) {
        let r = &self.records[i * RECORD..];
        (
            f32::from_le_bytes(r[0..4].try_into().unwrap()),
            f32::from_le_bytes(r[4..8].try_into().unwrap()),
        )
    }

    fn place(&self, i: usize) -> Place {
        let r = &self.records[i * RECORD..i * RECORD + RECORD];
        let region = u16::from_le_bytes(r[10..12].try_into().unwrap()) as usize;
        let name_at = u32::from_le_bytes(r[12..16].try_into().unwrap()) as usize;
        Place {
            name: &self.names[name_at..name_at + r[16] as usize],
            region: self.region(region),
            cc: std::str::from_utf8(&r[8..10]).unwrap_or(""),
        }
    }

    fn region(&self, i: usize) -> &'static str {
        let at = |i: usize| {
            u32::from_le_bytes(self.offsets[i * 4..i * 4 + 4].try_into().unwrap()) as usize
        };
        &self.regions[at(i)..at(i + 1)]
    }

    /// Nearest neighbour by squared degrees. The distortion away from the
    /// equator is irrelevant here: the answer only has to be the closest of
    /// 144k cities, not a distance anyone reads.
    fn nearest(&self, lat: f32, lon: f32) -> Option<usize> {
        let mut best = (f32::INFINITY, usize::MAX);
        self.descend(0, self.count, 0, lat, lon, &mut best);
        (best.1 != usize::MAX).then_some(best.1)
    }

    /// Walks the same implicit layout the generator wrote: the median of every
    /// range sits at its midpoint, split on latitude at even depths.
    fn descend(
        &self,
        lo: usize,
        hi: usize,
        depth: u32,
        lat: f32,
        lon: f32,
        best: &mut (f32, usize),
    ) {
        if hi <= lo {
            return;
        }
        let mid = lo + (hi - lo) / 2;
        let (plat, plon) = self.coords(mid);
        let d = (plat - lat).powi(2) + (plon - lon).powi(2);
        if d < best.0 {
            *best = (d, mid);
        }

        let delta = if depth % 2 == 0 {
            lat - plat
        } else {
            lon - plon
        };
        let (near, far) = if delta < 0.0 {
            ((lo, mid), (mid + 1, hi))
        } else {
            ((mid + 1, hi), (lo, mid))
        };
        self.descend(near.0, near.1, depth + 1, lat, lon, best);
        if delta * delta < best.0 {
            self.descend(far.0, far.1, depth + 1, lat, lon, best);
        }
    }
}

pub fn nearest(lat: f32, lon: f32) -> Option<Place> {
    let data = dataset();
    data.nearest(lat, lon).map(|i| data.place(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_blob_parses() {
        let data = dataset();
        assert!(data.count > 100_000, "only {} cities", data.count);
    }

    #[test]
    fn every_record_resolves_to_valid_text() {
        let data = dataset();
        for i in (0..data.count).step_by(997) {
            let (lat, lon) = data.coords(i);
            assert!((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon));
            let place = data.place(i);
            assert_eq!(place.cc.len(), 2);
            assert!(!place.name.is_empty());
        }
    }

    /// The k-d pruning is the only part that can silently return a wrong
    /// answer, so it is checked against an exhaustive scan.
    #[test]
    fn the_tree_agrees_with_a_linear_scan() {
        let data = dataset();
        for &(lat, lon) in &[
            (45.815, 15.982),
            (-33.87, 151.21),
            (40.71, -74.0),
            (78.22, 15.65),
            (0.0, 0.0),
            (-54.8, -68.3),
        ] {
            let mut brute = (f32::INFINITY, usize::MAX);
            for i in 0..data.count {
                let (plat, plon) = data.coords(i);
                let d = (plat - lat).powi(2) + (plon - lon).powi(2);
                if d < brute.0 {
                    brute = (d, i);
                }
            }
            let found = data.nearest(lat, lon).unwrap();
            assert_eq!(
                data.coords(found),
                data.coords(brute.1),
                "wrong neighbour for ({lat}, {lon})"
            );
        }
    }

    #[test]
    fn rejects_a_truncated_blob() {
        assert!(Dataset::load(&DATA[..HEADER + 3]).is_none());
        assert!(Dataset::load(b"nope").is_none());
    }
}
