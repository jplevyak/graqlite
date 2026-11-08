//! Vectors: values holding numbers, for nearest-neighbour search.
//!
//! Values are untyped bytes, and a vector is one of them: a header byte
//! naming the element type, then the elements.  The header is a byte that
//! UTF-8 never has (0xF8 to 0xFF), so no text is taken for a vector.  The
//! only element type is `f32`, little endian ([`F32`]); others, such as
//! quantized ones, can take other header bytes.
//!
//! - **Text:** a vector is written `[0.1, 0.2, 0.3]` ([`format()`]), and
//!   [`parse`] reads that, or the numbers alone, separated by commas or
//!   white space.
//! - **Distance:** [`distance`] by a [`Metric`]: smaller is nearer, for each.
//!
//! GraQL's `vector(x)` makes one from text (or is the vector given), `str`
//! writes one as text, and `distance(a, b[, metric])` measures between two
//! (see [`query::expr`](crate::query::expr)).

use std::fmt::Write;

/// The header byte of a vector of `f32`s, little endian.
pub const F32: u8 = 0xF8;

/// The most elements a vector may have.
pub const MAX_DIMS: usize = 65_536;

/// `elements` as a value's data.
pub fn encode(elements: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 4 * elements.len());
    out.push(F32);
    for x in elements {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Whether `data` is a vector's: its header, and whole elements, at least
/// one.
pub fn is_vector(data: &[u8]) -> bool {
    data.first() == Some(&F32) && data.len() > 1 && (data.len() - 1).is_multiple_of(4)
}

/// The elements of a vector's data, or `None` if it isn't one.
pub fn decode(data: &[u8]) -> Option<Vec<f32>> {
    if !is_vector(data) {
        return None;
    }
    Some(
        data[1..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&c| f32::from_le_bytes(c))
            .collect(),
    )
}

/// The vector `text` writes: numbers separated by commas or white space,
/// in brackets or not.  `None` if it is anything else, has no numbers or
/// too many, or a number that isn't finite as an `f32`.
pub fn parse(text: &str) -> Option<Vec<f32>> {
    let text = text.trim();
    let text = match text.strip_prefix('[') {
        Some(rest) => rest.strip_suffix(']')?,
        None => text,
    };
    let out = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<f32>().ok().filter(|x| x.is_finite()))
        .collect::<Option<Vec<f32>>>()?;
    (!out.is_empty() && out.len() <= MAX_DIMS).then_some(out)
}

/// `elements` as text: `[0.1, 0.2, 0.3]`, each the shortest that reads back
/// the same.
pub fn format(elements: &[f32]) -> String {
    let mut out = String::with_capacity(2 + 12 * elements.len());
    out.push('[');
    for (i, x) in elements.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write!(out, "{x}").expect("a string");
    }
    out.push(']');
    out
}

/// How [`distance`] measures: each is smaller for nearer vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Metric {
    /// One less the cosine of the angle between them: 0 for the same
    /// direction, 2 for opposite ones.
    Cosine,
    /// The Euclidean distance.
    L2,
    /// The negated dot product, so that a greater product is nearer.
    Dot,
}

impl Metric {
    pub fn of_name(name: &str) -> Option<Metric> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "cosine" => Metric::Cosine,
            "l2" | "euclidean" => Metric::L2,
            "dot" | "ip" => Metric::Dot,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Metric::Cosine => "cosine",
            Metric::L2 => "l2",
            Metric::Dot => "dot",
        }
    }
}

/// The dot product of `a` and `b`, in `f32`, in lanes the compiler can
/// vectorize: fast, for comparing distances; [`distance`] is exact.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut lanes = [0f32; 8];
    let ((ca, ra), (cb, rb)) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    for (x, y) in ca.iter().zip(cb) {
        for i in 0..8 {
            lanes[i] += x[i] * y[i];
        }
    }
    let tail: f32 = ra.iter().zip(rb).map(|(x, y)| x * y).sum();
    lanes.iter().sum::<f32>() + tail
}

/// The squared Euclidean distance between `a` and `b`, as [`dot`] is
/// summed.
pub fn l2_squared(a: &[f32], b: &[f32]) -> f32 {
    let mut lanes = [0f32; 8];
    let ((ca, ra), (cb, rb)) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    for (x, y) in ca.iter().zip(cb) {
        for i in 0..8 {
            let d = x[i] - y[i];
            lanes[i] += d * d;
        }
    }
    let tail: f32 = ra.iter().zip(rb).map(|(x, y)| (x - y) * (x - y)).sum();
    lanes.iter().sum::<f32>() + tail
}

/// The distance between `a` and `b` by `metric`, summed in `f64`.  `None` if
/// their dimensions differ, or for the cosine of a zero vector.
pub fn distance(a: &[f32], b: &[f32], metric: Metric) -> Option<f64> {
    if a.len() != b.len() {
        return None;
    }
    let pairs = a.iter().zip(b).map(|(&x, &y)| (f64::from(x), f64::from(y)));
    Some(match metric {
        Metric::L2 => pairs.map(|(x, y)| (x - y) * (x - y)).sum::<f64>().sqrt(),
        Metric::Dot => -pairs.map(|(x, y)| x * y).sum::<f64>(),
        Metric::Cosine => {
            let (mut dot, mut aa, mut bb) = (0.0, 0.0, 0.0);
            for (x, y) in pairs {
                dot += x * y;
                aa += x * x;
                bb += y * y;
            }
            if aa == 0.0 || bb == 0.0 {
                return None;
            }
            1.0 - dot / (aa.sqrt() * bb.sqrt())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes() {
        let v = [0.1, -2.5, 3.0];
        let data = encode(&v);
        assert_eq!(data[0], F32);
        assert_eq!(data.len(), 13);
        assert!(is_vector(&data));
        assert_eq!(decode(&data).unwrap(), v);
        // No text is a vector, and neither is a header alone or a part.
        assert!(!is_vector("[1, 2]".as_bytes()));
        assert!(std::str::from_utf8(&data).is_err());
        assert!(!is_vector(&[F32]));
        assert!(!is_vector(&data[..12]));
    }

    #[test]
    fn parses_and_formats() {
        assert_eq!(parse("[0.1, 0.2, 3]"), Some(vec![0.1, 0.2, 3.0]));
        assert_eq!(parse(" 1 2,3 "), Some(vec![1.0, 2.0, 3.0]));
        assert_eq!(parse("[1,,2]"), Some(vec![1.0, 2.0]));
        for bad in ["", "[]", "[1, 2", "1, x", "1e39", "NaN", "inf"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(format(&[0.1, -2.5, 3.0]), "[0.1, -2.5, 3]");
        let v = parse("[0.1, 0.2, 0.3]").unwrap();
        assert_eq!(parse(&format(&v)).unwrap(), v);
    }

    #[test]
    fn measures_distances() {
        let (a, b) = ([1.0, 0.0], [0.0, 2.0]);
        assert_eq!(distance(&a, &a, Metric::Cosine), Some(0.0));
        assert!((distance(&a, &b, Metric::Cosine).unwrap() - 1.0).abs() < 1e-12);
        assert!((distance(&a, &[-1.0, 0.0], Metric::Cosine).unwrap() - 2.0).abs() < 1e-12);
        assert!((distance(&a, &b, Metric::L2).unwrap() - 5f64.sqrt()).abs() < 1e-12);
        assert_eq!(distance(&[1.0, 2.0], &[3.0, 4.0], Metric::Dot), Some(-11.0));
        assert_eq!(distance(&a, &[1.0], Metric::L2), None);
        assert_eq!(distance(&a, &[0.0, 0.0], Metric::Cosine), None);
        assert_eq!(Metric::of_name("Cosine"), Some(Metric::Cosine));
        assert_eq!(Metric::of_name("manhattan"), None);
    }
}
