use std::{collections::BTreeSet, error::Error, fmt, ops::Deref};

/// Error returned when a value exceeds a protocol collection or byte limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundExceeded {
    pub limit: usize,
    pub actual: usize,
}

impl fmt::Display for BoundExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "bounded value contains {} items or bytes; limit is {}",
            self.actual, self.limit
        )
    }
}

impl Error for BoundExceeded {}

/// A vector whose maximum length is encoded in its type.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BoundedVec<T, const N: usize>(Vec<T>);

impl<T, const N: usize> BoundedVec<T, N> {
    pub const fn capacity_limit() -> usize {
        N
    }

    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn try_from_vec(values: Vec<T>) -> Result<Self, BoundExceeded> {
        if values.len() > N {
            return Err(BoundExceeded {
                limit: N,
                actual: values.len(),
            });
        }
        Ok(Self(values))
    }

    pub fn try_push(&mut self, value: T) -> Result<(), BoundExceeded> {
        if self.0.len() == N {
            return Err(BoundExceeded {
                limit: N,
                actual: self.0.len().saturating_add(1),
            });
        }
        self.0.push(value);
        Ok(())
    }

    pub fn into_vec(self) -> Vec<T> {
        self.0
    }
}

impl<T, const N: usize> Default for BoundedVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Deref for BoundedVec<T, N> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a BoundedVec<T, N> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<T, const N: usize> IntoIterator for BoundedVec<T, N> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// An ordered set whose maximum number of unique values is type-bounded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedSet<T, const N: usize>(BTreeSet<T>);

impl<T: Ord, const N: usize> BoundedSet<T, N> {
    pub fn new() -> Self {
        Self(BTreeSet::new())
    }

    pub fn try_from_iter(values: impl IntoIterator<Item = T>) -> Result<Self, BoundExceeded> {
        let values = values.into_iter().collect::<BTreeSet<_>>();
        if values.len() > N {
            return Err(BoundExceeded {
                limit: N,
                actual: values.len(),
            });
        }
        Ok(Self(values))
    }

    pub fn contains(&self, value: &T) -> bool {
        self.0.contains(value)
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn try_insert(&mut self, value: T) -> Result<bool, BoundExceeded> {
        if self.0.contains(&value) {
            return Ok(false);
        }
        if self.0.len() == N {
            return Err(BoundExceeded {
                limit: N,
                actual: self.0.len().saturating_add(1),
            });
        }
        Ok(self.0.insert(value))
    }
}

impl<T: Ord, const N: usize> Default for BoundedSet<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

/// UTF-8 text with an explicit byte limit.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BoundedText<const N: usize>(String);

impl<const N: usize> BoundedText<N> {
    pub fn try_new(value: impl Into<String>) -> Result<Self, BoundExceeded> {
        let value = value.into();
        if value.len() > N {
            return Err(BoundExceeded {
                limit: N,
                actual: value.len(),
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<const N: usize> fmt::Debug for BoundedText<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Opaque raw provider bytes with a hard size cap.
#[derive(Clone, Eq, PartialEq)]
pub struct BoundedBytes<const N: usize>(Vec<u8>);

impl<const N: usize> BoundedBytes<N> {
    pub fn try_new(bytes: impl Into<Vec<u8>>) -> Result<Self, BoundExceeded> {
        let bytes = bytes.into();
        if bytes.len() > N {
            return Err(BoundExceeded {
                limit: N,
                actual: bytes.len(),
            });
        }
        Ok(Self(bytes))
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl<const N: usize> fmt::Debug for BoundedBytes<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedBytes")
            .field("len", &self.0.len())
            .field("limit", &N)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_values_reject_overflow_without_truncating() {
        assert_eq!(
            BoundedVec::<_, 2>::try_from_vec(vec![1, 2, 3]),
            Err(BoundExceeded {
                limit: 2,
                actual: 3
            })
        );
        assert!(BoundedText::<3>::try_new("four").is_err());
        assert!(BoundedBytes::<2>::try_new([1, 2, 3]).is_err());
    }

    #[test]
    fn bounded_set_counts_unique_values() {
        let values = BoundedSet::<_, 2>::try_from_iter([1, 1, 2]).expect("bounded set");
        assert_eq!(values.iter().copied().collect::<Vec<_>>(), vec![1, 2]);
    }
}
