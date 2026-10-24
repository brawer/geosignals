// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Counts how often each pixel value occurs in the full-resolution image,
//! and renders the counts as a GDAL Raster Attribute Table.
//!
//! There is one bucket per pixel value, so the histogram is exact: with it,
//! readers can turn any pixel value into a percentile without scanning the
//! whole planet. Overviews are not counted.
//!
//! Bucket 0 holds every pixel without any nodes, which is most of the
//! planet. Zero is a real measurement, not missing data, so we set no
//! GDAL_NODATA value.

use crate::encode::MAX_VALUE;

pub struct Histogram {
    counts: Vec<u64>,
}

impl Default for Histogram {
    /// A histogram for node density, whose values go up to 4095.
    fn default() -> Self {
        Histogram::new(MAX_VALUE)
    }
}

impl Histogram {
    /// A histogram for values from 0 to `max_value`.
    pub fn new(max_value: u16) -> Self {
        Histogram {
            counts: vec![0; max_value as usize + 1],
        }
    }

    pub fn add(&mut self, pixels: &[u16]) {
        for &v in pixels {
            self.counts[v as usize] += 1;
        }
    }

    pub fn add_value(&mut self, v: u16, n: u64) {
        self.counts[v as usize] += n;
    }

    pub fn merge(&mut self, other: &Histogram) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += b;
        }
    }

    /// Smallest and largest value that occurs, or (0, 0) if none.
    pub fn range(&self) -> (u16, u16) {
        let min = self.counts.iter().position(|&c| c > 0).unwrap_or(0);
        let max = self.counts.iter().rposition(|&c| c > 0).unwrap_or(0);
        (min as u16, max as u16)
    }

    pub fn counts(&self) -> &[u64] {
        &self.counts
    }

    /// Renders the histogram as a band-1 Raster Attribute Table with
    /// linear binning, as an `<Item>` for the GDAL_METADATA TIFF tag.
    /// Rows go up to the largest value that occurs. GDAL 3.12+ and QGIS
    /// read this through GetDefaultRAT(); older readers ignore it.
    ///
    /// The count column is Real, not Integer, because GDAL's Integer is
    /// 32 bits and bucket 0 alone holds about 6.7·10¹⁰ pixels. Doubles
    /// represent every count exactly, since the image has only 2³⁶ pixels.
    /// Field codes are GDAL's: Real=1; usage Min=3, Max=4, PixelCount=1.
    pub fn rat_xml(&self) -> String {
        use std::fmt::Write;
        let n = self.counts.iter().rposition(|&c| c > 0).unwrap_or(0) + 1;
        let mut b = String::with_capacity(64 * (n + 4));
        b.push_str(r#"<Item name="DEFAULT_RASTER_ATTRIBUTE_TABLE" sample="0" role="rat">"#);
        b.push_str(r#"<GDALRasterAttributeTable Row0Min="0" BinSize="1" tableType="athematic">"#);
        push_fields(&mut b);
        for (i, c) in self.counts[..n].iter().enumerate() {
            write!(
                b,
                r#"<Row index="{i}"><F>{i}</F><F>{}</F><F>{c}</F></Row>"#,
                i + 1
            )
            .unwrap();
        }
        b.push_str("</GDALRasterAttributeTable></Item>");
        b
    }

    /// Like [`Histogram::rat_xml`], but with rows only for the values
    /// that occur, plus value 0. Without linear binning, readers find a
    /// value's row by its min and max columns. This is for 16-bit data
    /// with few distinct values, where one row per value up to the
    /// largest would take megabytes.
    pub fn sparse_rat_xml(&self) -> String {
        use std::fmt::Write;
        let rows = self
            .counts
            .iter()
            .enumerate()
            .filter(|&(v, &c)| v == 0 || c > 0);
        let mut b = String::new();
        b.push_str(r#"<Item name="DEFAULT_RASTER_ATTRIBUTE_TABLE" sample="0" role="rat">"#);
        b.push_str(r#"<GDALRasterAttributeTable tableType="athematic">"#);
        push_fields(&mut b);
        for (i, (v, c)) in rows.enumerate() {
            write!(
                b,
                r#"<Row index="{i}"><F>{v}</F><F>{}</F><F>{c}</F></Row>"#,
                v + 1
            )
            .unwrap();
        }
        b.push_str("</GDALRasterAttributeTable></Item>");
        b
    }
}

fn push_fields(b: &mut String) {
    b.push_str(
        r#"<FieldDefn index="0"><Name>min</Name><Type>1</Type><Usage>3</Usage></FieldDefn>"#,
    );
    b.push_str(
        r#"<FieldDefn index="1"><Name>max</Name><Type>1</Type><Usage>4</Usage></FieldDefn>"#,
    );
    b.push_str(
        r#"<FieldDefn index="2"><Name>count</Name><Type>1</Type><Usage>1</Usage></FieldDefn>"#,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_merges() {
        let mut a = Histogram::default();
        a.add(&[0, 0, 3, 4095]);
        let mut b = Histogram::default();
        b.add(&[3]);
        b.add_value(0, 1 << 40);
        a.merge(&b);
        assert_eq!(a.counts()[0], (1 << 40) + 2);
        assert_eq!(a.counts()[3], 2);
        assert_eq!(a.counts()[4095], 1);
        assert_eq!(a.counts().iter().sum::<u64>(), (1 << 40) + 5);
        assert_eq!(a.range(), (0, 4095));
        let mut c = Histogram::default();
        assert_eq!(c.range(), (0, 0));
        c.add(&[7, 9]);
        assert_eq!(c.range(), (7, 9));
    }

    #[test]
    fn rat_xml_stops_at_largest_value() {
        let mut h = Histogram::default();
        h.add(&[0, 2, 2]);
        h.add_value(0, 1 << 36);
        let xml = h.rat_xml();
        assert!(xml.contains(r#"<Row index="0"><F>0</F><F>1</F><F>68719476737</F></Row>"#));
        assert!(xml.contains(r#"<Row index="1"><F>1</F><F>2</F><F>0</F></Row>"#));
        assert!(xml.contains(r#"<Row index="2"><F>2</F><F>3</F><F>2</F></Row>"#));
        assert!(!xml.contains(r#"<Row index="3">"#));
        assert!(xml.ends_with("</Item>"));
    }

    #[test]
    fn sparse_rat_xml_skips_missing_values() {
        let mut h = Histogram::new(u16::MAX);
        h.add(&[65535, 3, 3, 0]);
        let xml = h.sparse_rat_xml();
        assert!(!xml.contains("Row0Min"));
        assert!(xml.contains(r#"<Row index="0"><F>0</F><F>1</F><F>1</F></Row>"#));
        assert!(xml.contains(r#"<Row index="1"><F>3</F><F>4</F><F>2</F></Row>"#));
        assert!(xml.contains(r#"<Row index="2"><F>65535</F><F>65536</F><F>1</F></Row>"#));
        assert!(!xml.contains(r#"<Row index="3">"#));
        assert_eq!(h.range(), (0, 65535));
    }

    #[test]
    fn rat_xml_of_empty_histogram_has_one_row() {
        let xml = Histogram::default().rat_xml();
        assert!(xml.contains(r#"<Row index="0"><F>0</F><F>1</F><F>0</F></Row>"#));
        assert!(!xml.contains(r#"<Row index="1">"#));
    }
}
