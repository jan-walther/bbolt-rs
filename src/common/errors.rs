use std::io;
use thiserror::Error;

/// Bolt database errors
#[derive(Error, Debug)]
pub enum Error {
  // These errors can be returned when opening or calling methods on a DB.
  /// DatabaseNotOpen is returned when a DB instance is accessed before it
  /// is opened or after it is closed.
  #[error("database not open")]
  DatabaseNotOpen,
  /// DatabaseOpen is returned when opening a database that is
  /// already open.
  #[error("database already open")]
  DatabaseOpen,
  /// InvalidDatabase is returned when both meta pages on a database are invalid.
  /// This typically occurs when a file is not a bolt database.
  #[error("invalid database - meta_can_read: `{0}`")]
  InvalidDatabase(bool),
  /// InvalidMapping is returned when the database file fails to get mapped.
  #[error("database isn't correctly mapped")]
  InvalidMapping,
  /// ErrVersionMismatch is returned when the data file was created with a
  /// different version of Bolt.
  #[error("version mismatch")]
  VersionMismatch,
  /// Checksum is returned when either meta page checksum does not match.
  #[error("checksum mismatch")]
  ChecksumMismatch,
  /// File size is below the minimum size a Bolt database could be
  #[error("file size too small: `{0}`")]
  FileSizeTooSmall(u64),
  /// Timeout is returned when a database cannot obtain an exclusive lock
  /// on the data file after the timeout passed to Open().
  #[error("timeout")]
  Timeout,
  /// FreePagesNotLoaded is returned when a readonly transaction without
  /// preloading the free pages is trying to access the free pages.
  #[error("free pages are not pre-loaded")]
  FreePagesNotLoaded,
  // These errors can occur when putting or deleting a value or a bucket.
  /// BucketNotFound is returned when trying to access a bucket that has
  /// not been created yet.
  #[error("bucket not found")]
  BucketNotFound,
  /// BucketExists is returned when creating a bucket that already exists.
  #[error("bucket already exists")]
  BucketExists,
  /// BucketNameRequired is returned when creating a bucket with a blank name.
  #[error("bucket name required")]
  BucketNameRequired,
  /// KeyRequired is returned when inserting a zero-length key.
  #[error("key required")]
  KeyRequired,
  /// KeyTooLarge is returned when inserting a key that is larger than MaxKeySize.
  #[error("key too large")]
  KeyTooLarge,
  /// ValueTooLarge is returned when inserting a value that is larger than MaxValueSize.
  #[error("value too large")]
  ValueTooLarge,
  /// IncompatibleValue is returned when trying to create or delete a bucket
  /// on an existing non-bucket key or when trying to create or delete a
  /// non-bucket key on an existing bucket key.
  #[error("incompatible value")]
  IncompatibleValue,
  /// File size is below the minimum size a Bolt database could be
  #[error("mmap too small: `{0}`")]
  MMapTooSmall(u64),
  /// File size is larger than the maximum size a Bolt database could be
  #[error("mmap too large")]
  MMapTooLarge,
  /// Batch call failed. Try again without batch
  #[error("try function without batch call")]
  TrySolo,
  /// Batch is disabled
  #[error("max batch delay or length is set to 0")]
  BatchDisabled,
  /// Chained errors from other sources
  #[error(transparent)]
  IO(#[from] io::Error),
  /// User defined error
  #[error(transparent)]
  Other(#[from] anyhow::Error),
}

impl PartialEq for Error {
  fn eq(&self, other: &Self) -> bool {
    matches!(
      (self, other),
      (&Error::DatabaseNotOpen, &Error::DatabaseNotOpen)
        | (Error::DatabaseOpen, Error::DatabaseOpen)
        | (Error::InvalidDatabase(_), Error::InvalidDatabase(_))
        | (Error::InvalidMapping, Error::InvalidMapping)
        | (Error::VersionMismatch, Error::VersionMismatch)
        | (Error::ChecksumMismatch, Error::ChecksumMismatch)
        | (Error::FileSizeTooSmall(_), Error::FileSizeTooSmall(_))
        | (Error::Timeout, Error::Timeout)
        | (Error::FreePagesNotLoaded, Error::FreePagesNotLoaded)
        | (Error::BucketNotFound, Error::BucketNotFound)
        | (Error::BucketExists, Error::BucketExists)
        | (Error::BucketNameRequired, Error::BucketNameRequired)
        | (Error::KeyRequired, Error::KeyRequired)
        | (Error::KeyTooLarge, Error::KeyTooLarge)
        | (Error::ValueTooLarge, Error::ValueTooLarge)
        | (Error::IncompatibleValue, Error::IncompatibleValue)
        | (Error::MMapTooSmall(_), Error::MMapTooSmall(_))
        | (Error::MMapTooLarge, Error::MMapTooLarge)
        | (Error::TrySolo, Error::TrySolo)
        | (Error::BatchDisabled, Error::BatchDisabled)
    )
  }
}

impl Eq for Error {}

pub type Result<T, E = Error> = std::result::Result<T, E>;
