use std::collections::HashMap;
use std::sync::Arc;

use crate::utils::{delete_dir, wait_for_future, walk_tree};
use crate::PyDeltaTableError;

use deltalake::storage::{
    DynObjectStore, GetResult, ListResult, MultipartId, ObjectMeta, ObjectStore, ObjectStoreError,
    ObjectStoreResult, Path,
};
use deltalake::DeltaTableBuilder;
use futures::stream::poll_fn;
use pyo3::exceptions::{PyIOError, PyNotImplementedError, PyValueError, PyFileNotFoundError};
use pyo3::prelude::*;
use pyo3::types::{IntoPyDict, PyBytes};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use chrono::{DateTime, NaiveDateTime};
use bytes::Bytes;
use std::ops::Range;
use futures::task::Poll;
use futures::future::BoxFuture;
use async_trait::async_trait;
use futures::stream::BoxStream;

#[pyclass(subclass)]
#[derive(Debug, Clone)]
pub struct DeltaFileSystemHandler {
    pub(crate) inner: Arc<DynObjectStore>,
}

#[pymethods]
impl DeltaFileSystemHandler {
    #[new]
    #[args(options = "None")]
    fn new(table_uri: &str, options: Option<HashMap<String, String>>) -> PyResult<Self> {
        let storage = DeltaTableBuilder::from_uri(table_uri)
            .with_storage_options(options.unwrap_or_default())
            .build_storage()
            .map_err(PyDeltaTableError::from_raw)?;
        Ok(Self { inner: storage })
    }

    fn get_type_name(&self) -> String {
        "object-store".into()
    }

    fn normalize_path(&self, path: String) -> PyResult<String> {
        let suffix = if path.ends_with('/') { "/" } else { "" };
        let path = Path::parse(path).unwrap();
        Ok(format!("{}{}", path, suffix))
    }

    fn copy_file(&self, src: String, dest: String, py: Python) -> PyResult<()> {
        let from_path = Path::from(src);
        let to_path = Path::from(dest);
        wait_for_future(py, self.inner.copy(&from_path, &to_path))
            .map_err(PyDeltaTableError::from_object_store)?;
        Ok(())
    }

    fn create_dir(&self, _path: String, _recursive: bool) -> PyResult<()> {
        // TODO creating a dir should be a no-op with object_store, right?
        Ok(())
    }

    fn delete_dir(&self, path: String, py: Python) -> PyResult<()> {
        let path = Path::from(path);
        wait_for_future(py, delete_dir(self.inner.as_ref(), &path))
            .map_err(PyDeltaTableError::from_object_store)?;
        Ok(())
    }

    fn delete_file(&self, path: String, py: Python) -> PyResult<()> {
        let path = Path::from(path);
        wait_for_future(py, self.inner.delete(&path))
            .map_err(PyDeltaTableError::from_object_store)?;
        Ok(())
    }

    fn equals(&self, other: &DeltaFileSystemHandler) -> PyResult<bool> {
        Ok(format!("{:?}", self) == format!("{:?}", other))
    }

    fn get_file_info<'py>(&self, paths: Vec<String>, py: Python<'py>) -> PyResult<Vec<&'py PyAny>> {
        let fs = PyModule::import(py, "pyarrow.fs")?;
        let file_types = fs.getattr("FileType")?;

        let to_file_info = |loc: String, type_: &PyAny, kwargs: HashMap<&str, i64>| {
            fs.call_method("FileInfo", (loc, type_), Some(kwargs.into_py_dict(py)))
        };

        let mut infos = Vec::new();
        for file_path in paths {
            let path = Path::from(file_path);
            let listed = wait_for_future(py, self.inner.list_with_delimiter(Some(&path)))
                .map_err(PyDeltaTableError::from_object_store)?;

            // TODO is there a better way to figure out if we are in a directory?
            if listed.objects.is_empty() && listed.common_prefixes.is_empty() {
                let maybe_meta = wait_for_future(py, self.inner.head(&path));
                match maybe_meta {
                    Ok(meta) => {
                        let kwargs = HashMap::from([
                            ("size", meta.size as i64),
                            ("mtime_ns", meta.last_modified.timestamp_nanos()),
                        ]);
                        infos.push(to_file_info(
                            meta.location.to_string(),
                            file_types.getattr("File")?,
                            kwargs,
                        )?);
                    }
                    Err(ObjectStoreError::NotFound { .. }) => {
                        infos.push(to_file_info(
                            path.to_string(),
                            file_types.getattr("NotFound")?,
                            HashMap::new(),
                        )?);
                    }
                    Err(err) => {
                        return Err(PyDeltaTableError::from_object_store(err));
                    }
                }
            } else {
                infos.push(to_file_info(
                    path.to_string(),
                    file_types.getattr("Directory")?,
                    HashMap::new(),
                )?);
            }
        }

        Ok(infos)
    }

    #[args(allow_not_found = "false", recursive = "false")]
    fn get_file_info_selector<'py>(
        &self,
        base_dir: String,
        allow_not_found: bool,
        recursive: bool,
        py: Python<'py>,
    ) -> PyResult<Vec<&'py PyAny>> {
        let fs = PyModule::import(py, "pyarrow.fs")?;
        let file_types = fs.getattr("FileType")?;

        let to_file_info = |loc: String, type_: &PyAny, kwargs: HashMap<&str, i64>| {
            fs.call_method("FileInfo", (loc, type_), Some(kwargs.into_py_dict(py)))
        };

        let path = Path::from(base_dir);
        let list_result =
            match wait_for_future(py, walk_tree(self.inner.clone(), &path, recursive)) {
                Ok(res) => Ok(res),
                Err(ObjectStoreError::NotFound { path, source }) => {
                    if allow_not_found {
                        Ok(ListResult {
                            common_prefixes: vec![],
                            objects: vec![],
                        })
                    } else {
                        Err(ObjectStoreError::NotFound { path, source })
                    }
                }
                Err(err) => Err(err),
            }
            .map_err(PyDeltaTableError::from_object_store)?;

        let mut infos = vec![];
        infos.extend(
            list_result
                .common_prefixes
                .into_iter()
                .map(|p| {
                    to_file_info(
                        p.to_string(),
                        file_types.getattr("Directory")?,
                        HashMap::new(),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
        infos.extend(
            list_result
                .objects
                .into_iter()
                .map(|meta| {
                    let kwargs = HashMap::from([
                        ("size", meta.size as i64),
                        ("mtime_ns", meta.last_modified.timestamp_nanos()),
                    ]);
                    to_file_info(
                        meta.location.to_string(),
                        file_types.getattr("File")?,
                        kwargs,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?,
        );

        Ok(infos)
    }

    fn move_file(&self, src: String, dest: String, py: Python) -> PyResult<()> {
        let from_path = Path::from(src);
        let to_path = Path::from(dest);
        // TODO check the if not exists semantics
        wait_for_future(py, self.inner.rename(&from_path, &to_path))
            .map_err(PyDeltaTableError::from_object_store)?;
        Ok(())
    }

    fn open_input_file(&self, path: String, py: Python) -> PyResult<ObjectInputFile> {
        let path = Path::from(path);
        let file = wait_for_future(py, ObjectInputFile::try_new(self.inner.clone(), path))
            .map_err(PyDeltaTableError::from_object_store)?;
        Ok(file)
    }

    #[args(metadata = "None")]
    fn open_output_stream(
        &self,
        path: String,
        #[allow(unused)] metadata: Option<HashMap<String, String>>,
        py: Python,
    ) -> PyResult<ObjectOutputStream> {
        let path = Path::from(path);
        let file = wait_for_future(py, ObjectOutputStream::try_new(self.inner.clone(), path))
            .map_err(PyDeltaTableError::from_object_store)?;
        Ok(file)
    }
}

// TODO the C++ implementation track an internal lock on all random access files, DO we need this here?
// TODO add buffer to store data ...
#[pyclass(weakref)]
#[derive(Debug, Clone)]
pub struct ObjectInputFile {
    store: Arc<DynObjectStore>,
    path: Path,
    content_length: i64,
    #[pyo3(get)]
    closed: bool,
    pos: i64,
    #[pyo3(get)]
    mode: String,
}

impl ObjectInputFile {
    pub async fn try_new(store: Arc<DynObjectStore>, path: Path) -> Result<Self, ObjectStoreError> {
        // Issue a HEAD Object to get the content-length and ensure any
        // errors (e.g. file not found) don't wait until the first read() call.
        let meta = store.head(&path).await?;
        let content_length = meta.size as i64;
        // TODO make sure content length is valid
        // https://github.com/apache/arrow/blob/f184255cbb9bf911ea2a04910f711e1a924b12b8/cpp/src/arrow/filesystem/s3fs.cc#L1083
        Ok(Self {
            store,
            path,
            content_length,
            closed: false,
            pos: 0,
            mode: "rb".into(),
        })
    }

    fn check_closed(&self) -> PyResult<()> {
        if self.closed {
            return Err(PyIOError::new_err("Operation on closed stream"));
        }

        Ok(())
    }

    fn check_position(&self, position: i64, action: &str) -> PyResult<()> {
        if position < 0 {
            return Err(PyIOError::new_err(format!(
                "Cannot {} for negative position.",
                action
            )));
        }
        if position > self.content_length {
            return Err(PyIOError::new_err(format!(
                "Cannot {} past end of file.",
                action
            )));
        }
        Ok(())
    }
}

#[pymethods]
impl ObjectInputFile {
    fn close(&mut self) -> PyResult<()> {
        self.closed = true;
        Ok(())
    }

    fn isatty(&self) -> PyResult<bool> {
        Ok(false)
    }

    fn readable(&self) -> PyResult<bool> {
        Ok(true)
    }

    fn seekable(&self) -> PyResult<bool> {
        Ok(true)
    }

    fn writable(&self) -> PyResult<bool> {
        Ok(false)
    }

    fn tell(&self) -> PyResult<i64> {
        self.check_closed()?;
        Ok(self.pos)
    }

    fn size(&self) -> PyResult<i64> {
        self.check_closed()?;
        Ok(self.content_length)
    }

    #[args(whence = "0")]
    fn seek(&mut self, offset: i64, whence: i64) -> PyResult<i64> {
        self.check_closed()?;
        self.check_position(offset, "seek")?;
        match whence {
            // reference is start of the stream (the default); offset should be zero or positive
            0 => {
                self.pos = offset;
            }
            // reference is current stream position; offset may be negative
            1 => {
                self.pos += offset;
            }
            // reference is  end of the stream; offset is usually negative
            2 => {
                self.pos = self.content_length as i64 + offset;
            }
            _ => {
                return Err(PyValueError::new_err(
                    "'whence' must be between  0 <= whence <= 2.",
                ));
            }
        }
        Ok(self.pos)
    }

    #[args(nbytes = "None")]
    fn read<'py>(&mut self, nbytes: Option<i64>, py: Python<'py>) -> PyResult<&'py PyBytes> {
        self.check_closed()?;
        let range = match nbytes {
            Some(len) => {
                let end = i64::min(self.pos + len, self.content_length) as usize;
                std::ops::Range {
                    start: self.pos as usize,
                    end,
                }
            }
            _ => std::ops::Range {
                start: self.pos as usize,
                end: self.content_length as usize,
            },
        };
        let nbytes = (range.end - range.start) as i64;
        self.pos += nbytes;
        let obj = if nbytes > 0 {
            wait_for_future(py, self.store.get_range(&self.path, range))
                .map_err(PyDeltaTableError::from_object_store)?
                .to_vec()
        } else {
            Vec::new()
        };
        Ok(PyBytes::new(py, &obj))
    }

    fn fileno(&self) -> PyResult<()> {
        Err(PyNotImplementedError::new_err("'fileno' not implemented"))
    }

    fn truncate(&self) -> PyResult<()> {
        Err(PyNotImplementedError::new_err("'truncate' not implemented"))
    }

    fn readline(&self, _size: Option<i64>) -> PyResult<()> {
        Err(PyNotImplementedError::new_err("'readline' not implemented"))
    }

    fn readlines(&self, _hint: Option<i64>) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(
            "'readlines' not implemented",
        ))
    }
}

// TODO the C++ implementation track an internal lock on all random access files, DO we need this here?
// TODO add buffer to store data ...
#[pyclass(weakref)]
pub struct ObjectOutputStream {
    store: Arc<DynObjectStore>,
    path: Path,
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    multipart_id: MultipartId,
    pos: i64,
    #[pyo3(get)]
    closed: bool,
    #[pyo3(get)]
    mode: String,
}

impl ObjectOutputStream {
    pub async fn try_new(store: Arc<DynObjectStore>, path: Path) -> Result<Self, ObjectStoreError> {
        let (multipart_id, writer) = store.put_multipart(&path).await.unwrap();
        Ok(Self {
            store,
            path,
            writer,
            multipart_id,
            pos: 0,
            closed: false,
            mode: "wb".into(),
        })
    }

    fn check_closed(&self) -> PyResult<()> {
        if self.closed {
            return Err(PyIOError::new_err("Operation on closed stream"));
        }

        Ok(())
    }
}

#[pymethods]
impl ObjectOutputStream {
    fn close(&mut self, py: Python) -> PyResult<()> {
        self.closed = true;
        match wait_for_future(py, self.writer.shutdown()) {
            Ok(_) => Ok(()),
            Err(err) => {
                wait_for_future(
                    py,
                    self.store.abort_multipart(&self.path, &self.multipart_id),
                )
                .map_err(PyDeltaTableError::from_object_store)?;
                Err(PyDeltaTableError::from_io(err))
            }
        }
    }

    fn isatty(&self) -> PyResult<bool> {
        Ok(false)
    }

    fn readable(&self) -> PyResult<bool> {
        Ok(false)
    }

    fn seekable(&self) -> PyResult<bool> {
        Ok(false)
    }

    fn writable(&self) -> PyResult<bool> {
        Ok(true)
    }

    fn tell(&self) -> PyResult<i64> {
        self.check_closed()?;
        Ok(self.pos)
    }

    fn size(&self) -> PyResult<i64> {
        self.check_closed()?;
        Err(PyNotImplementedError::new_err("'size' not implemented"))
    }

    #[args(whence = "0")]
    fn seek(&mut self, _offset: i64, _whence: i64) -> PyResult<i64> {
        self.check_closed()?;
        Err(PyNotImplementedError::new_err("'seek' not implemented"))
    }

    #[args(nbytes = "None")]
    fn read(&mut self, _nbytes: Option<i64>) -> PyResult<()> {
        self.check_closed()?;
        Err(PyNotImplementedError::new_err("'read' not implemented"))
    }

    fn write(&mut self, data: Vec<u8>, py: Python) -> PyResult<i64> {
        self.check_closed()?;
        let len = data.len() as i64;
        match wait_for_future(py, self.writer.write_all(&data)) {
            Ok(_) => Ok(len),
            Err(err) => {
                wait_for_future(
                    py,
                    self.store.abort_multipart(&self.path, &self.multipart_id),
                )
                .map_err(PyDeltaTableError::from_object_store)?;
                Err(PyDeltaTableError::from_io(err))
            }
        }
    }

    fn flush(&mut self, py: Python) -> PyResult<()> {
        match wait_for_future(py, self.writer.flush()) {
            Ok(_) => Ok(()),
            Err(err) => {
                wait_for_future(
                    py,
                    self.store.abort_multipart(&self.path, &self.multipart_id),
                )
                .map_err(PyDeltaTableError::from_object_store)?;
                Err(PyDeltaTableError::from_io(err))
            }
        }
    }

    fn fileno(&self) -> PyResult<()> {
        Err(PyNotImplementedError::new_err("'fileno' not implemented"))
    }

    fn truncate(&self) -> PyResult<()> {
        Err(PyNotImplementedError::new_err("'truncate' not implemented"))
    }

    fn readline(&self, _size: Option<i64>) -> PyResult<()> {
        Err(PyNotImplementedError::new_err("'readline' not implemented"))
    }

    fn readlines(&self, _hint: Option<i64>) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(
            "'readlines' not implemented",
        ))
    }
}

/// PyArrow filesystem wrapped as an ObjectStore
#[derive(Debug)]
#[pyclass(module = "deltalake.fs", text_signature = "(py_store, root)")]
struct WrappedPyArrowStore {
    py_store: PyObject,
}

#[pymethods]
impl WrappedPyArrowStore {
    #[new]
    pub fn new(py_store: PyObject, root: Option<&str>, py: Python) -> PyResult<Self> {
        let pa_fs = PyModule::import(py, "pyarrow.fs")?;
        let pa_filesystem = pa_fs.getattr("FileSystem")?;
        let pa_subtreefilesystem = pa_fs.getattr("SubTreeFileSystem")?;

        // TODO: handle fsspec here too?
        
        if !py_store.as_ref(py).is_instance(pa_filesystem.get_type()) {
            return Err(PyValueError::new_err("Must pass a PyArrow filesystem."));
        }

        if !py_store.as_ref(py).is_instance(pa_subtreefilesystem.get_type()) {
            py_store = pa_subtreefilesystem.call1((root, py_store))?;
        }

        Ok(WrappedPyArrowStore { py_store })
    }

    /// The inner filesystem
    ///
    /// :rtype: pyarrow.fs.FileSystem
    #[getter]
    fn inner(&self) -> PyObject {
        self.py_store
    }

    fn __repr__(&self, py: Python) -> PyResult<String> {
        Ok(format!(
            "WrappedPyArrowStore({})",
            self.py_store.call_method0(py, "__repr__")?
        ))
    }
}

impl std::fmt::Display for WrappedPyArrowStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WrappedPyArrowStore()")
    }
}

fn store_error_from_python(path: String, py_error: PyErr) -> ObjectStoreError {
    Python::with_gil(|py| {
        let pyarrow = PyModule::import(py, "pyarrow").map_err(|err| ObjectStoreError::Generic {
            store: "pyarrow",
            source: Box::new(err),
        })?;
        let arrow_not_implemented_error =
            pyarrow
                .get_attr("ArrowNotImplementedError")
                .map_err(|err| ObjectStoreError::Generic {
                    store: "pyarrow",
                    source: Box::new(err),
                })?;

        if py_error.get_type(py).is_instance::<PyFileNotFoundError>(py) {
            ObjectStoreError::NotFound {
                path,
                source: Box::new(py_error),
            }
        } else if py_error.into_py(py).as_ref(py).is_instance(arrow_not_implemented_error.get_type()) {
            ObjectStoreError::NotImplemented
        } else {
            ObjectStoreError::Generic {
                store: "pyarrow",
                source: Box::new(py_error),
            }
        }
    })
}

#[async_trait]
impl ObjectStore for WrappedPyArrowStore {
    async fn put(&self, location: &Path, bytes: Bytes) -> ObjectStoreResult<()> {
        let path = location.to_string();
        Python::with_gil(|py| {
            let out_stream = self
                .py_store
                .call_method1(py, "open_output_stream", path)?;
            out_stream.call_method1(py, "write", bytes)?;
            out_stream.call_method0(py, "close")?;
            Ok(())
        })
        .map_err(|err| store_error_from_python(path, err))
    }

    async fn get(&self, location: &Path) -> ObjectStoreResult<GetResult> {
        let path = location.to_string();
        let in_stream = Python::with_gil(|py| {
            let in_stream = self
                .py_store
                .call_method1(py, "open_input_stream", path)?;
            Ok(in_stream)
        })
        .map_err(|err| store_error_from_python(path, err))?;

        let current_read: Option<BoxFuture> = None;

        let read_stream = poll_fn(move |cx| -> Poll<Option<Bytes>> {
            if current_read.is_none() {
                current_read = tokio::runtime::Runtime::current().block_on(|| -> ObjectStoreResult<Bytes> {
                    Python::with_gil(|py| -> PyResult<Bytes> {
                        in_stream
                            .call_method1(py, "read", 5 * 1024 * 1024)?
                            .extract(py)
                    })
                    .map_err(|err| store_error_from_python(path, err))
                })
            }

            if let Some(read_fut) = current_read {
                let res = read_fut.poll(cx);
                if let Poll::Ready(_) = res {
                    current_read = None;
                }
                res
            }
        });

        Ok(GetResult::Stream(Box::pin(read_stream)))
    }

    async fn get_range(&self, location: &Path, range: Range<usize>) -> ObjectStoreResult<Bytes> {
        // TODO: use read_at()
        Err(ObjectStoreError::NotImplemented)
    }

    async fn head(&self, location: &Path) -> ObjectStoreResult<ObjectMeta> {
        let path = location.to_string();
        Python::with_gil(|py| {
            let info = self.py_store.call_method1(py, "get_file_info", (path,))?;
            let last_modified = if info.getattr(py, "mtime_ns")?.is_none(py) {
                let last_modified: i64 = info.getattr(py, "mtime")?.extract(py)?;
                DateTime::<chrono::Utc>::from_utc(NaiveDateTime::from_timestamp(last_modified, 0), chrono::Utc)
            } else {
                let last_modified: i64 = info.getattr(py, "mtime_ns")?.extract(py)?;
                let seconds = last_modified / 1_000_000;
                let nanoseconds: u32 = last_modified % 1_000_000;
                DateTime::<chrono::Utc>::from_utc(NaiveDateTime::from_timestamp(seconds, nanoseconds), chrono::Utc)
            };
            Ok(ObjectMeta {
                location: Path::from(info.getattr(py, "path")?.extract(py))?,
                last_modified,
                size: info.getattr(py, "size")?.extract(py)?,
            })
        })
        .map_err(|err| store_error_from_python(path, err))
    }

    async fn delete(&self, location: &Path) -> ObjectStoreResult<()> {
        Err(ObjectStoreError::NotImplemented)
    }

    async fn list(
        &self,
        prefix: Option<&Path>,
    ) -> ObjectStoreResult<BoxStream<'_, ObjectStoreResult<ObjectMeta>>> {
        Err(ObjectStoreError::NotImplemented)
    }
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> ObjectStoreResult<ListResult> {
        Err(ObjectStoreError::NotImplemented)
    }

    async fn copy(&self, from: &Path, to: &Path) -> ObjectStoreResult<()> {
        Err(ObjectStoreError::NotImplemented)
    }
    async fn copy_if_not_exists(&self, from: &Path, to: &Path) -> ObjectStoreResult<()> {
        Err(ObjectStoreError::NotImplemented)
    }

    async fn rename_if_not_exists(&self, from: &Path, to: &Path) -> ObjectStoreResult<()> {
        Err(ObjectStoreError::NotImplemented)
    }

    async fn put_multipart(
        &self,
        location: &Path,
    ) -> ObjectStoreResult<(MultipartId, Box<dyn AsyncWrite + Unpin + Send>)> {
        Err(ObjectStoreError::NotImplemented)
    }

    async fn abort_multipart(
        &self,
        location: &Path,
        multipart_id: &MultipartId,
    ) -> ObjectStoreResult<()> {
        Err(ObjectStoreError::NotImplemented)
    }
}

fn try_unwrap_object_store(
    root: String,
    py_store: PyObject,
    py: Python,
) -> PyResult<Arc<dyn ObjectStore>> {
    let pa_fs = PyModule::import(py, "pyarrow.fs")?;
    let pa_filesystem = pa_fs.getattr("FileSystem")?;

    let deltalake = PyModule::import(py, "deltalake.fs")?;
    let delta_storage_handler = deltalake.getattr("DeltaStorageHandler")?;

    if py_store.as_ref(py).is_instance(pa_filesystem.get_type()) {
        Ok(Arc::new(WrappedPyArrowStore::new(py_store, Some(&root), py)))
    } else if py_store.as_ref(py).is_instance(delta_storage_handler.get_type()) {
        let inner: DeltaFileSystemHandler = py_store.extract(py)?;
        Ok(Arc::clone(&inner.inner))
    } else {
        Err(PyValueError::new_err(
            "Filesystem must be a pyarrow.FileSystem or DeltaStorageHandler.",
        ))
    }
}
