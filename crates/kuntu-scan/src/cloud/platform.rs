use super::model::{Fingerprint, ItemInfo, Result};
use std::path::Path;

pub trait CloudBackend: Send + Sync {
  fn inspect(&self, path: &Path) -> Result<ItemInfo>;
  fn evict_local_copy(&self, path: &Path, expected: &Fingerprint) -> Result<()>;
  fn request_download(&self, path: &Path, expected: &Fingerprint) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NativeICloudBackend;

#[cfg(not(target_os = "macos"))]
impl CloudBackend for NativeICloudBackend {
  fn inspect(&self, _path: &Path) -> Result<ItemInfo> {
    Err(super::model::CloudError::new(
      super::model::ErrorKind::UnsupportedPlatform,
      "native iCloud operations require macOS",
    ))
  }

  fn evict_local_copy(&self, _path: &Path, _expected: &Fingerprint) -> Result<()> {
    Err(super::model::CloudError::new(
      super::model::ErrorKind::UnsupportedPlatform,
      "native iCloud operations require macOS",
    ))
  }

  fn request_download(&self, _path: &Path, _expected: &Fingerprint) -> Result<()> {
    Err(super::model::CloudError::new(
      super::model::ErrorKind::UnsupportedPlatform,
      "native iCloud operations require macOS",
    ))
  }
}

#[cfg(target_os = "macos")]
mod macos {
  use super::super::guard::{allocated_bytes_for_path, fingerprint_for_path};
  use super::super::model::{
    Bytes, CloudError, DownloadState, ErrorKind, Fingerprint, ItemInfo, ItemKind, NativeError,
    Result,
  };
  use super::super::policy::eviction_skip_reason;
  use super::{CloudBackend, NativeICloudBackend};
  use objc2::rc::{autoreleasepool, Retained};
  use objc2::runtime::AnyObject;
  use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSFileManager, NSNumber, NSString, NSURLFileAllocatedSizeKey,
    NSURLIsPackageKey, NSURLTotalFileAllocatedSizeKey, NSURLUbiquitousItemDownloadingErrorKey,
    NSURLUbiquitousItemDownloadingStatusCurrent, NSURLUbiquitousItemDownloadingStatusDownloaded,
    NSURLUbiquitousItemDownloadingStatusKey, NSURLUbiquitousItemDownloadingStatusNotDownloaded,
    NSURLUbiquitousItemHasUnresolvedConflictsKey, NSURLUbiquitousItemIsDownloadingKey,
    NSURLUbiquitousItemIsUploadedKey, NSURLUbiquitousItemIsUploadingKey,
    NSURLUbiquitousItemUploadingErrorKey, NSURL,
  };
  use std::path::Path;

  type Values = NSDictionary<NSString, AnyObject>;

  struct Keys {
    package: &'static NSString,
    status: &'static NSString,
    uploaded: &'static NSString,
    uploading: &'static NSString,
    downloading: &'static NSString,
    conflicts: &'static NSString,
    upload_error: &'static NSString,
    download_error: &'static NSString,
    allocated: &'static NSString,
    total_allocated: &'static NSString,
  }

  impl Keys {
    fn load() -> Self {
      unsafe {
        Self {
          package: NSURLIsPackageKey,
          status: NSURLUbiquitousItemDownloadingStatusKey,
          uploaded: NSURLUbiquitousItemIsUploadedKey,
          uploading: NSURLUbiquitousItemIsUploadingKey,
          downloading: NSURLUbiquitousItemIsDownloadingKey,
          conflicts: NSURLUbiquitousItemHasUnresolvedConflictsKey,
          upload_error: NSURLUbiquitousItemUploadingErrorKey,
          download_error: NSURLUbiquitousItemDownloadingErrorKey,
          allocated: NSURLFileAllocatedSizeKey,
          total_allocated: NSURLTotalFileAllocatedSizeKey,
        }
      }
    }
  }

  fn file_url(path: &Path) -> Result<Retained<NSURL>> {
    NSURL::from_file_path(path).ok_or_else(|| {
      CloudError::new(
        ErrorKind::InvalidPath,
        format!("path cannot be represented as an NSURL: {}", path.display()),
      )
    })
  }

  fn native_error(error: &NSError) -> NativeError {
    NativeError {
      domain: error.domain().to_string(),
      code: error.code() as i64,
      description: error.localizedDescription().to_string(),
    }
  }

  fn bool_value(values: &Values, key: &NSString) -> Result<Option<bool>> {
    let Some(value) = values.objectForKey(key) else {
      return Ok(None);
    };
    value
      .downcast_ref::<NSNumber>()
      .map(|number| Some(number.boolValue()))
      .ok_or_else(|| {
        CloudError::new(
          ErrorKind::InvalidState,
          format!("unexpected value type for {key}"),
        )
      })
  }

  fn bytes_value(values: &Values, key: &NSString) -> Result<Option<Bytes>> {
    let Some(value) = values.objectForKey(key) else {
      return Ok(None);
    };
    value
      .downcast_ref::<NSNumber>()
      .map(|number| Some(Bytes(number.unsignedLongLongValue())))
      .ok_or_else(|| {
        CloudError::new(
          ErrorKind::InvalidState,
          format!("unexpected byte-count type for {key}"),
        )
      })
  }

  fn error_value(values: &Values, key: &NSString) -> Result<Option<NativeError>> {
    let Some(value) = values.objectForKey(key) else {
      return Ok(None);
    };
    value
      .downcast_ref::<NSError>()
      .map(|error| Some(native_error(error)))
      .ok_or_else(|| {
        CloudError::new(
          ErrorKind::InvalidState,
          format!("unexpected error value for {key}"),
        )
      })
  }

  fn download_state(values: &Values, key: &NSString) -> Result<DownloadState> {
    let Some(value) = values.objectForKey(key) else {
      return Ok(DownloadState::Unknown);
    };
    let status = value.downcast_ref::<NSString>().ok_or_else(|| {
      CloudError::new(
        ErrorKind::InvalidState,
        "unexpected ubiquitous downloading-status type",
      )
    })?;
    let (cloud_only, current, downloaded) = unsafe {
      (
        NSURLUbiquitousItemDownloadingStatusNotDownloaded,
        NSURLUbiquitousItemDownloadingStatusCurrent,
        NSURLUbiquitousItemDownloadingStatusDownloaded,
      )
    };
    Ok(if status.isEqualToString(cloud_only) {
      DownloadState::CloudOnly
    } else if status.isEqualToString(current) {
      DownloadState::LocalCurrent
    } else if status.isEqualToString(downloaded) {
      DownloadState::LocalStale
    } else {
      DownloadState::Unknown
    })
  }

  impl CloudBackend for NativeICloudBackend {
    fn inspect(&self, path: &Path) -> Result<ItemInfo> {
      autoreleasepool(|_| {
        let before = fingerprint_for_path(path)?;
        if before.kind == ItemKind::SymbolicLink {
          return Err(CloudError::new(
            ErrorKind::SymbolicLink,
            format!("do not inspect a symbolic link: {}", path.display()),
          ));
        }

        let url = file_url(path)?;
        url.removeAllCachedResourceValues();
        let file_manager = NSFileManager::defaultManager();
        let keys = Keys::load();
        let is_icloud = file_manager.isUbiquitousItemAtURL(&url);
        let mut info = ItemInfo {
          fingerprint: before.clone(),
          allocated_bytes: Some(allocated_bytes_for_path(path)?),
          foundation_allocated_bytes: None,
          foundation_total_allocated_bytes: None,
          is_icloud,
          is_package: if before.kind == ItemKind::RegularFile {
            Some(false)
          } else {
            None
          },
          download_state: DownloadState::Unknown,
          is_uploaded: None,
          is_uploading: None,
          is_downloading: None,
          has_conflicts: None,
          upload_error: None,
          download_error: None,
        };

        if before.kind == ItemKind::Directory {
          let values = url
            .resourceValuesForKeys_error(&NSArray::from_slice(&[keys.package]))
            .map_err(|error| CloudError::native(native_error(&error)))?;
          info.is_package = bool_value(&values, keys.package)?;
        } else if before.kind == ItemKind::RegularFile && is_icloud {
          let values = url
            .resourceValuesForKeys_error(&NSArray::from_slice(&[
              keys.status,
              keys.uploaded,
              keys.uploading,
              keys.downloading,
              keys.conflicts,
              keys.upload_error,
              keys.download_error,
              keys.allocated,
              keys.total_allocated,
            ]))
            .map_err(|error| CloudError::native(native_error(&error)))?;
          info.download_state = download_state(&values, keys.status)?;
          info.is_uploaded = bool_value(&values, keys.uploaded)?;
          info.is_uploading = bool_value(&values, keys.uploading)?;
          info.is_downloading = bool_value(&values, keys.downloading)?;
          info.has_conflicts = bool_value(&values, keys.conflicts)?;
          info.upload_error = error_value(&values, keys.upload_error)?;
          info.download_error = error_value(&values, keys.download_error)?;
          info.foundation_allocated_bytes = bytes_value(&values, keys.allocated)?;
          info.foundation_total_allocated_bytes = bytes_value(&values, keys.total_allocated)?;
        }

        let after = fingerprint_for_path(path)?;
        if after != before {
          return Err(CloudError::new(
            ErrorKind::ChangedSincePlan,
            format!(
              "file changed during metadata inspection: {}",
              path.display()
            ),
          ));
        }
        Ok(info)
      })
    }

    fn evict_local_copy(&self, path: &Path, expected: &Fingerprint) -> Result<()> {
      let fresh = self.inspect(path)?;
      if &fresh.fingerprint != expected {
        return Err(CloudError::new(
          ErrorKind::ChangedSincePlan,
          format!(
            "file changed immediately before eviction: {}",
            path.display()
          ),
        ));
      }
      if let Some(reason) = eviction_skip_reason(&fresh) {
        return Err(CloudError::new(
          ErrorKind::InvalidState,
          format!("eviction rejected: {reason}"),
        ));
      }

      autoreleasepool(|_| {
        let url = file_url(path)?;
        NSFileManager::defaultManager()
          .evictUbiquitousItemAtURL_error(&url)
          .map_err(|error| CloudError::native(native_error(&error)))
      })
    }

    fn request_download(&self, path: &Path, expected: &Fingerprint) -> Result<()> {
      let fresh = self.inspect(path)?;
      if &fresh.fingerprint != expected {
        return Err(CloudError::new(
          ErrorKind::ChangedSincePlan,
          format!("file changed before download request: {}", path.display()),
        ));
      }
      if !fresh.is_icloud
        || fresh.kind() != ItemKind::RegularFile
        || fresh.is_uploaded != Some(true)
        || fresh.is_uploading != Some(false)
        || fresh.has_conflicts != Some(false)
      {
        return Err(CloudError::new(
          ErrorKind::InvalidState,
          "download request rejected by safety policy",
        ));
      }

      autoreleasepool(|_| {
        let url = file_url(path)?;
        NSFileManager::defaultManager()
          .startDownloadingUbiquitousItemAtURL_error(&url)
          .map_err(|error| CloudError::native(native_error(&error)))
      })
    }
  }
}
