use nix::unistd::{Gid, Uid};
use std::cmp::Ordering;
use std::ops::{Bound, RangeBounds};

pub trait Id: Copy + Eq {
    fn from_raw(raw: u32) -> Self;

    fn as_raw(self) -> u32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct IdRangeInner<T: Id> {
    pub(super) start: T,
    pub(super) end: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IdRange<T: Id> {
    pub(super) range: Option<IdRangeInner<T>>,
}

pub type UidRange = IdRange<Uid>;

pub type GidRange = IdRange<Gid>;

mod inner {
    use nix::unistd::{Gid, Uid};

    pub fn uid_from_raw(raw: u32) -> Uid {
        Uid::from_raw(raw)
    }

    pub fn uid_as_raw(uid: Uid) -> u32 {
        uid.as_raw()
    }

    pub fn gid_from_raw(raw: u32) -> Gid {
        Gid::from_raw(raw)
    }

    pub fn gid_as_raw(gid: Gid) -> u32 {
        gid.as_raw()
    }
}

impl Id for Uid {
    fn from_raw(raw: u32) -> Self {
        inner::uid_from_raw(raw)
    }

    fn as_raw(self) -> u32 {
        inner::uid_as_raw(self)
    }
}

impl Id for Gid {
    fn from_raw(raw: u32) -> Self {
        inner::gid_from_raw(raw)
    }

    fn as_raw(self) -> u32 {
        inner::gid_as_raw(self)
    }
}

impl<T: Id> Default for IdRange<T> {
    fn default() -> Self {
        Self::empty()
    }
}

impl<T: Id> IdRange<T> {
    pub fn empty() -> Self {
        Self { range: None }
    }

    pub fn new(start: T, end: T) -> Self {
        Self::from(start..=end)
    }

    pub fn is_empty(self) -> bool {
        self.range.is_none()
    }

    pub fn len(self) -> usize {
        match self.range {
            None => 0,
            Some(range) => ((range.end.as_raw() - range.start.as_raw()) + 1) as usize,
        }
    }

    pub fn start(self) -> Option<T> {
        self.range.map(|range| range.start)
    }

    pub fn end(self) -> Option<T> {
        self.range.map(|range| range.end)
    }

    pub fn iter(self) -> Iter<T> {
        self.into_iter()
    }
}

#[derive(Debug)]
pub struct Iter<T: Id> {
    curr: u32,
    count: usize,
    _phantom: std::marker::PhantomData<fn() -> T>,
}

impl<T: Id> Iterator for Iter<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.count == 0 {
            None
        } else {
            let id = self.curr;
            self.curr += 1;
            self.count -= 1;
            Some(T::from_raw(id))
        }
    }
}

impl<T: Id> IntoIterator for IdRange<T> {
    type Item = T;
    type IntoIter = Iter<T>;

    fn into_iter(self) -> Iter<T> {
        (&self).into_iter()
    }
}

impl<T: Id> IntoIterator for &mut IdRange<T> {
    type Item = T;
    type IntoIter = Iter<T>;

    fn into_iter(self) -> Iter<T> {
        (&*self).into_iter()
    }
}

impl<T: Id> IntoIterator for &IdRange<T> {
    type Item = T;
    type IntoIter = Iter<T>;

    fn into_iter(self) -> Iter<T> {
        match self.range {
            None => Iter {
                curr: 0,
                count: 0,
                _phantom: std::marker::PhantomData,
            },
            Some(range) => Iter {
                curr: range.start.as_raw(),
                count: ((range.end.as_raw() - range.start.as_raw()) + 1) as usize,
                _phantom: std::marker::PhantomData,
            },
        }
    }
}

impl<T: Id> PartialOrd for IdRange<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(Ord::cmp(self, other))
    }
}

impl<T: Id> Ord for IdRange<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        match (&self.range, &other.range) {
            (None, None) => Ordering::Equal,
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (Some(l), Some(r)) => Ord::cmp(l, r),
        }
    }
}

impl<T: Id> PartialOrd for IdRangeInner<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(Ord::cmp(self, other))
    }
}

impl<T: Id> Ord for IdRangeInner<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        match Ord::cmp(&self.end.as_raw(), &other.start.as_raw()) {
            Ordering::Less => Ordering::Less,
            Ordering::Equal => Ordering::Equal,
            Ordering::Greater => match Ord::cmp(&self.start.as_raw(), &other.end.as_raw()) {
                Ordering::Greater => Ordering::Greater,
                Ordering::Less | Ordering::Equal => Ordering::Equal,
            },
        }
    }
}

impl<T: Id> IdRange<T> {
    fn from_raw_range<R>(range: R) -> Self
    where
        R: RangeBounds<u32>,
    {
        Self::from_range((
            range.start_bound().map(|id| T::from_raw(*id)),
            range.start_bound().map(|id| T::from_raw(*id)),
        ))
    }

    fn from_range<R>(range: R) -> Self
    where
        R: RangeBounds<T>,
    {
        match (range.start_bound().cloned(), range.end_bound().cloned()) {
            (Bound::Unbounded, Bound::Unbounded) => Self { range: None },
            (Bound::Unbounded, Bound::Included(id)) => Self {
                range: Some(IdRangeInner {
                    start: T::from_raw(0),
                    end: id,
                }),
            },
            (Bound::Unbounded, Bound::Excluded(id)) => {
                if id.as_raw() == 0 {
                    Self { range: None }
                } else {
                    Self {
                        range: Some(IdRangeInner {
                            start: T::from_raw(0),
                            end: T::from_raw(id.as_raw() - 1),
                        }),
                    }
                }
            }
            (Bound::Included(id), Bound::Unbounded) => Self {
                range: Some(IdRangeInner {
                    start: id,
                    end: T::from_raw(0),
                }),
            },
            (Bound::Excluded(id), Bound::Unbounded) => {
                if id.as_raw() == u32::MAX {
                    Self { range: None }
                } else {
                    Self {
                        range: Some(IdRangeInner {
                            start: T::from_raw(id.as_raw() + 1),
                            end: T::from_raw(u32::MAX),
                        }),
                    }
                }
            }
            (Bound::Included(start), Bound::Included(end)) => {
                match Ord::cmp(&start.as_raw(), &end.as_raw()) {
                    Ordering::Less | Ordering::Equal => Self {
                        range: Some(IdRangeInner { start, end }),
                    },
                    Ordering::Greater => Self {
                        range: Some(IdRangeInner {
                            start: end,
                            end: start,
                        }),
                    },
                }
            }
            (Bound::Included(start), Bound::Excluded(end)) => {
                match Ord::cmp(&start.as_raw(), &end.as_raw()) {
                    Ordering::Less => Self {
                        range: Some(IdRangeInner {
                            start,
                            end: T::from_raw(end.as_raw() - 1),
                        }),
                    },
                    Ordering::Equal => Self { range: None },
                    Ordering::Greater => Self {
                        range: Some(IdRangeInner {
                            start: T::from_raw(end.as_raw() + 1),
                            end: start,
                        }),
                    },
                }
            }
            (Bound::Excluded(start), Bound::Included(end)) => {
                match Ord::cmp(&start.as_raw(), &end.as_raw()) {
                    Ordering::Less => Self {
                        range: Some(IdRangeInner {
                            start: T::from_raw(start.as_raw() + 1),
                            end,
                        }),
                    },
                    Ordering::Equal => Self { range: None },
                    Ordering::Greater => Self {
                        range: Some(IdRangeInner {
                            start: end,
                            end: T::from_raw(start.as_raw() - 1),
                        }),
                    },
                }
            }
            (Bound::Excluded(start), Bound::Excluded(end)) => {
                match Ord::cmp(&start.as_raw(), &end.as_raw()) {
                    Ordering::Less => match Ord::cmp(&(start.as_raw() + 1), &(end.as_raw() - 1)) {
                        Ordering::Less | Ordering::Equal => Self {
                            range: Some(IdRangeInner {
                                start: T::from_raw(start.as_raw() + 1),
                                end: T::from_raw(end.as_raw() - 1),
                            }),
                        },
                        Ordering::Greater => Self { range: None },
                    },
                    Ordering::Equal => Self { range: None },
                    Ordering::Greater => match Ord::cmp(&(end.as_raw() + 1), &(start.as_raw() - 1))
                    {
                        Ordering::Less | Ordering::Equal => Self {
                            range: Some(IdRangeInner {
                                start: T::from_raw(end.as_raw() + 1),
                                end: T::from_raw(start.as_raw() - 1),
                            }),
                        },
                        Ordering::Greater => Self { range: None },
                    },
                }
            }
        }
    }
}

impl<T: Id> From<u32> for IdRange<T> {
    fn from(raw_id: u32) -> Self {
        Self::from(T::from_raw(raw_id))
    }
}

impl<T: Id> From<T> for IdRange<T> {
    fn from(id: T) -> Self {
        Self {
            range: Some(IdRangeInner { start: id, end: id }),
        }
    }
}

macro_rules! impl_from_range {
    ($($t:ty),* $(,)?) => {
        $(
            impl<T: Id> From<$t> for IdRange<T> {
                fn from(range: $t) -> Self {
                    Self::from_range(range)
                }
            }
        )*
    };
}

macro_rules! impl_from_raw_range {
    ($($t:ty),* $(,)?) => {
        $(
            impl<T: Id> From<$t> for IdRange<T> {
                fn from(range: $t) -> Self {
                    Self::from_raw_range(range)
                }
            }
        )*
    };
}

impl_from_range!(
    std::ops::RangeFull,
    std::ops::Range<T>,
    std::ops::RangeFrom<T>,
    std::ops::RangeTo<T>,
    std::ops::RangeInclusive<T>,
    std::ops::RangeToInclusive<T>,
    (std::ops::Bound<T>, std::ops::Bound<T>),
    std::ops::Range<&T>,
    std::ops::RangeFrom<&T>,
    std::ops::RangeTo<&T>,
    std::ops::RangeInclusive<&T>,
    std::ops::RangeToInclusive<&T>,
    (std::ops::Bound<&T>, std::ops::Bound<&T>),
);

impl_from_raw_range!(
    std::ops::Range<u32>,
    std::ops::RangeFrom<u32>,
    std::ops::RangeTo<u32>,
    std::ops::RangeInclusive<u32>,
    std::ops::RangeToInclusive<u32>,
    (std::ops::Bound<u32>, std::ops::Bound<u32>),
    std::ops::Range<&u32>,
    std::ops::RangeFrom<&u32>,
    std::ops::RangeTo<&u32>,
    std::ops::RangeInclusive<&u32>,
    std::ops::RangeToInclusive<&u32>,
    (std::ops::Bound<&u32>, std::ops::Bound<&u32>),
);
