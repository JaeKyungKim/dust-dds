use crate::infrastructure::time::Time;
use crate::transport::types::Guid;
use core::ops::Index;
use dust_dds_derive::TypeSupport;

pub use crate::transport::types::{SAMPLE_IDENTITY_UNKNOWN, SampleIdentity};

/// Optional parameters that refine a single write operation.
///
/// `WriteParams` is passed to
/// [`DataWriter::write_w_params`](crate::publication::data_writer::DataWriter::write_w_params)
/// (and its async counterpart) to carry a source timestamp, a
/// pre-resolved instance handle, and — most importantly for DDS-RPC —
/// a `related_sample_identity` that marks the written sample as a reply
/// to an earlier request.
///
/// `WriteParams::default()` is equivalent to the implicit parameters of
/// [`DataWriter::write`](crate::publication::data_writer::DataWriter::write):
/// no timestamp override, no explicit handle, and no RELATED_SAMPLE_IDENTITY
/// parameter on the wire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteParams {
    timestamp: Option<Time>,
    handle: Option<InstanceHandle>,
    related_sample_identity: Option<SampleIdentity>,
}

impl WriteParams {
    #[inline]
    pub const fn new() -> Self {
        Self {
            timestamp: None,
            handle: None,
            related_sample_identity: None,
        }
    }

    #[inline]
    pub fn with_timestamp(mut self, timestamp: Time) -> Self {
        self.timestamp = Some(timestamp);
        self
    }

    #[inline]
    pub fn with_handle(mut self, handle: InstanceHandle) -> Self {
        self.handle = Some(handle);
        self
    }

    #[inline]
    pub fn with_handle_opt(mut self, handle: Option<InstanceHandle>) -> Self {
        self.handle = handle;
        self
    }

    #[inline]
    pub fn with_related_sample_identity(mut self, id: SampleIdentity) -> Self {
        self.related_sample_identity = Some(id);
        self
    }

    #[inline]
    pub fn timestamp(&self) -> Option<Time> {
        self.timestamp
    }

    #[inline]
    pub fn handle(&self) -> Option<InstanceHandle> {
        self.handle
    }

    #[inline]
    pub fn related_sample_identity(&self) -> Option<SampleIdentity> {
        self.related_sample_identity
    }
}

/// Special constant value representing a 'nil' [`InstanceHandle`].
pub const HANDLE_NIL: InstanceHandle = InstanceHandle([0; 16]);

/// Type for the instance handle representing an Entity.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord, TypeSupport)]
pub struct InstanceHandle([u8; 16]);

impl InstanceHandle {
    /// Constructs a new `InstanceHandle`.
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

impl From<InstanceHandle> for [u8; 16] {
    #[inline]
    fn from(value: InstanceHandle) -> Self {
        value.0
    }
}

impl From<Guid> for InstanceHandle {
    #[inline]
    fn from(value: Guid) -> Self {
        Self(value.into())
    }
}

impl From<crate::rtps::behavior_types::InstanceHandle> for InstanceHandle {
    #[inline]
    fn from(value: crate::rtps::behavior_types::InstanceHandle) -> Self {
        Self(value.0)
    }
}

impl From<InstanceHandle> for crate::rtps::behavior_types::InstanceHandle {
    #[inline]
    fn from(value: InstanceHandle) -> Self {
        Self(value.0)
    }
}

impl PartialEq<[u8; 16]> for InstanceHandle {
    #[inline]
    fn eq(&self, other: &[u8; 16]) -> bool {
        self.0.eq(other)
    }
}

impl PartialEq<InstanceHandle> for [u8; 16] {
    #[inline]
    fn eq(&self, other: &InstanceHandle) -> bool {
        self.eq(&other.0)
    }
}

impl AsRef<[u8; 16]> for InstanceHandle {
    #[inline]
    fn as_ref(&self) -> &[u8; 16] {
        &self.0
    }
}

impl Default for InstanceHandle {
    #[inline]
    fn default() -> Self {
        HANDLE_NIL
    }
}

impl Index<usize> for InstanceHandle {
    type Output = u8;

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::types::{EntityId, USER_DEFINED_WRITER_WITH_KEY};

    #[test]
    fn write_params_default_is_all_none() {
        let params = WriteParams::default();
        assert_eq!(params.timestamp(), None);
        assert_eq!(params.handle(), None);
        assert_eq!(params.related_sample_identity(), None);
        assert_eq!(params, WriteParams::new());
    }

    #[test]
    fn write_params_builder_chain_preserves_fields() {
        let ts = Time::new(1, 2);
        let handle = InstanceHandle::new([7; 16]);
        let sid = SampleIdentity::new(
            Guid::new(
                [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
                EntityId::new([0x00, 0x00, 0x12], USER_DEFINED_WRITER_WITH_KEY),
            ),
            42,
        );
        let params = WriteParams::default()
            .with_timestamp(ts)
            .with_handle(handle)
            .with_related_sample_identity(sid);
        assert_eq!(params.timestamp(), Some(ts));
        assert_eq!(params.handle(), Some(handle));
        assert_eq!(params.related_sample_identity(), Some(sid));
    }

    #[test]
    fn write_params_with_handle_opt_none_clears_handle() {
        let params = WriteParams::default()
            .with_handle(InstanceHandle::new([1; 16]))
            .with_handle_opt(None);
        assert_eq!(params.handle(), None);
    }
}
