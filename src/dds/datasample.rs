use enumflags2::BitFlags;

use crate::{dds::traits::key::*, structure::guid::GUID};
use crate::structure::time::Timestamp;

use super::{interfaces::{IDataSample, IDataSampleConvert, IKeyedDataSample, IKeyedDataSampleConvert}, no_key::wrappers::NoKeyWrapper};

/// DDS spec 2.2.2.5.4
/// "Read" indicates whether or not the corresponding data sample has already been read.
#[derive(BitFlags, Debug, Copy, Clone, PartialEq)]
#[repr(u32)] // DDS Spec 1.4 section 2.3.3 DCPS PSM : IDL defines these as "unsigned long", so u32
pub enum SampleState {
  Read = 0b0001,
  NotRead = 0b0010,
}

impl SampleState {
  /// Set that contains all possible states
  pub fn any() -> BitFlags<Self> {
    BitFlags::<Self>::all()
  }
}

/// DDS spec 2.2.2.5.1.8
///
#[derive(BitFlags, Debug, Copy, Clone, PartialEq)]
#[repr(u32)]
pub enum ViewState {
  ///  indicates that either this is the first time that the DataReader has ever
  /// accessed samples of that instance, or else that the DataReader has accessed previous
  /// samples of the instance, but the instance has since been reborn (i.e., become
  /// not-alive and then alive again).
  New = 0b0001,
  /// indicates that the DataReader has already accessed samples of the same
  ///instance and that the instance has not been reborn since
  NotNew = 0b0010,
}
impl ViewState {
  /// Set that contains all possible states
  pub fn any() -> BitFlags<Self> {
    BitFlags::<Self>::all()
  }
}

#[derive(BitFlags, Debug, Copy, Clone, PartialEq)]
#[repr(u32)]
pub enum InstanceState {
  Alive = 0b0001,
  /// A DataWriter has actively disposed this instance
  NotAlive_Disposed = 0b0010,
  /// There are no writers alive.
  NotAlive_NoWriters = 0b0100,
}

impl InstanceState {
  /// Set that contains all possible states
  pub fn any() -> BitFlags<Self> {
    BitFlags::<Self>::all()
  }
  /// Set that contains both not_alive states.
  pub fn not_alive() -> BitFlags<Self> {
    InstanceState::NotAlive_Disposed | InstanceState::NotAlive_NoWriters
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SampleInfo {
  pub sample_state: SampleState,
  pub view_state: ViewState,
  pub instance_state: InstanceState,
  // For each instance the middleware internally maintains these counts relative
  // to each DataReader. The counts capture snapshots if the corresponding
  // counters at the time the sample was received.
  pub disposed_generation_count: i32,
  pub no_writers_generation_count: i32,
  // The ranks are are computed based solely on the actual samples in the
  // ordered collection returned by the read or take.
  // The sample_rank indicates the number of samples of the same instance that
  // follow the current one in the collection.
  pub sample_rank: i32,
  // The generation_rank indicates the difference in generations between the
  // samples S and the Most Recent Sample of the same instance that appears In
  // the returned Collection (MRSIC). It counts the number of times the instance
  // transitioned from not-alive to alive in the time from the reception of the
  // S to the  reception of MRSIC. The generation rank is computed with:
  // generation_rank =
  //(MRSIC.disposed_generation_count + MRSIC.no_writers_generation_count)
  //- (S.disposed_generation_count + S.no_writers_generation_count)
  pub generation_rank: i32,
  // The absolute_generation_rank indicates the difference in "generations"
  // between sample S and the Most Recent Sample of the instance that the
  // middlware has received (MRS). It counts the number of times the instance
  // transitioned from not-alive to alive in the time from the reception of the
  // S to the time when the read or take was called. absolute_generation_rank =
  //(MRS.disposed_generation_count + MRS.no_writers_generation_count)
  //- (S.disposed_generation_count + S.no_writers_generation_count)
  pub absolute_generation_rank: i32,
  pub source_timestamp: Timestamp,

  pub publication_handle: GUID,
}

#[allow(clippy::new_without_default)]
impl SampleInfo {
  pub fn new() -> Self {
    Self {
      sample_state: SampleState::NotRead,
      view_state: ViewState::New,
      instance_state: InstanceState::Alive,
      disposed_generation_count: 0,
      no_writers_generation_count: 0,
      sample_rank: 0,
      generation_rank: 0,
      absolute_generation_rank: 0,
      source_timestamp: Timestamp::TIME_INVALID,
      publication_handle: GUID::GUID_UNKNOWN,
    }
  }
}

/// DDS spec 2.2.2.5.4
#[derive(PartialEq, Debug)]
pub struct DataSample<D: Keyed> {
  sample_info: SampleInfo, // TODO: Can we somehow make this lazily evaluated?

  /// This ia a bit unorthodox use of Result.
  /// It replaces the use of valid_data flag, because when valid_data = false, we should
  /// not provide any data value.
  /// Now Ok(D) means valid_data = true and there is a sample.
  /// Err(D::K) means there is valid_data = false, but only a Key and instance_state has changed.
  value: std::result::Result<D, D::K>,
}

impl<D> DataSample<D>
where
  D: Keyed,
{
  pub fn new(source_timestamp: Timestamp, payload: D, writer_guid: GUID) -> DataSample<D> {
    // begin dummy placeholder values
    let sample_state = SampleState::NotRead;
    let view_state = ViewState::New;
    let instance_state = InstanceState::Alive;
    let disposed_generation_count = 0;
    let no_writers_generation_count = 0;
    let sample_rank = 0;
    let generation_rank = 0;
    let absolute_generation_rank = 0;
    // end dummy placeholder values

    DataSample {
      sample_info: SampleInfo {
        sample_state,
        view_state,
        instance_state,
        disposed_generation_count,
        no_writers_generation_count,
        sample_rank,
        generation_rank,
        absolute_generation_rank,
        source_timestamp,
        publication_handle: writer_guid,
      },
      value: Ok(payload),
    }
  }

  pub fn new_disposed<K>(source_timestamp: Timestamp, key: D::K, writer_guid: GUID) -> DataSample<D>
  where
    <D as Keyed>::K: Key,
  {
    // begin dummy placeholder values
    let sample_state = SampleState::NotRead;
    let view_state = ViewState::New;
    let instance_state = InstanceState::NotAlive_Disposed;
    let disposed_generation_count = 0;
    let no_writers_generation_count = 0;
    let sample_rank = 0;
    let generation_rank = 0;
    let absolute_generation_rank = 0;
    // end dummy placeholder values

    DataSample {
      sample_info: SampleInfo {
        sample_state,
        view_state,
        instance_state,
        disposed_generation_count,
        no_writers_generation_count,
        sample_rank,
        generation_rank,
        absolute_generation_rank,
        source_timestamp,
        publication_handle: writer_guid,
      },
      value: Err(key),
    }
  } // fn

  // convenience shorthand to get the key directly, without digging out the "value"
  pub fn get_key(&self) -> D::K
  where
    <D as Keyed>::K: Key,
  {
    match &self.value {
      Ok(d) => d.get_key(),
      Err(k) => k.clone(),
    }
  } // fn
} // imlp

impl<D: Keyed + 'static> IDataSampleConvert<D> for DataSample<D> {
  fn as_idata_sample(&self) -> &dyn IDataSample<D> {
    self
  }

  fn into_idata_sample(self) -> Box<dyn IDataSample<D>> {
    Box::new(self)
  }
}

impl<D: Keyed + 'static> IDataSample<D> for DataSample<D> {
  fn get_sample_info(&self) -> &SampleInfo {
    &self.sample_info
  }

  fn get_sample_info_mut(&mut self) -> &mut SampleInfo {
    &mut self.sample_info
  }

  fn get_value(&self) -> Option<&D> {
    match &self.value {
      Ok(d) => Some(d),
      _ => None,
    }
  }

  fn into_value(self) -> Option<D> {
    match self.value {
      Ok(d) => Some(d),
      _ => None,
    }
  }
}

impl<D: Keyed + 'static> IKeyedDataSampleConvert<D> for DataSample<D> {
  fn as_ikeyed_data_sample(&self) -> &dyn IKeyedDataSample<D> {
    self
  }

  fn into_ikeyed_data_sample(self) -> Box<dyn IKeyedDataSample<D>> {
    Box::new(self)
  }
}

impl<D: Keyed + 'static> IKeyedDataSample<D> for DataSample<D> {
  fn get_keyed_value(&self) -> &Result<D, D::K> {
    &self.value
  }
}

impl<D: 'static> IDataSampleConvert<D> for DataSample<NoKeyWrapper<D>> {
  fn as_idata_sample(&self) -> &dyn IDataSample<D> {
    self
  }

  fn into_idata_sample(self) -> Box<dyn IDataSample<D>> {
    Box::new(self)
  }
}

impl<D: 'static> IDataSample<D> for DataSample<NoKeyWrapper<D>> {
  fn get_sample_info(&self) -> &SampleInfo {
    &self.sample_info
  }

  fn get_sample_info_mut(&mut self) -> &mut SampleInfo {
    &mut self.sample_info
  }

  fn get_value(&self) -> Option<&D> {
    match &self.value {
      Ok(v) => Some(&v),
      _ => None,
    }
  }

  fn into_value(self) -> Option<D> {
    match self.value {
      Ok(v) => Some(v.unwrap()),
      _ => None,
    }
  }
}
