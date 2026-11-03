//! The thin Python binding over the switchyard crate.
//!
//! Every rule lives in the library this crate wraps. The functions
//! here translate Python values into the library arguments, call
//! the one library function each one names, and translate the
//! result or the refusal back, and nothing else. Each call runs
//! detached from the interpreter, so the filesystem and lock and
//! git work inside a call never stalls the other threads of the
//! calling process while it waits. A library refusal
//! becomes a Python exception carrying the same message text the
//! command line prints, under a subclass of SwitchyardError picked
//! by the exit class of the refusal, so a Python caller can tell a
//! validation failure from a plain refusal from a corrupt registry
//! the way a shell caller tells them apart by exit code.

use std::path::PathBuf;

use pyo3::create_exception;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use switchyard_core::{Error, Record};

create_exception!(switchyard, SwitchyardError, pyo3::exceptions::PyException);
create_exception!(switchyard, RefusalError, SwitchyardError);
create_exception!(switchyard, NotInitialisedError, SwitchyardError);
create_exception!(switchyard, CorruptRegistryError, SwitchyardError);
create_exception!(switchyard, InvalidNameError, SwitchyardError);

/// Translate a library refusal into the Python exception of its
/// exit class, carrying the message the command line prints.
fn to_py_error(error: Error) -> PyErr {
    let message = error.message().to_string();
    match error {
        Error::Plain(_) | Error::Io(_) => RefusalError::new_err(message),
        Error::NotInitialised(_) => NotInitialisedError::new_err(message),
        Error::Corrupt(_) => CorruptRegistryError::new_err(message),
        Error::InvalidName(_) => InvalidNameError::new_err(message),
    }
}

/// Translate one recorded claim into a plain dictionary carrying
/// every field the registry record carries.
fn record_dict(py: Python<'_>, record: &Record) -> PyResult<Py<PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("agent", &record.agent)?;
    dict.set_item("branch", &record.branch)?;
    dict.set_item("worktree", &record.worktree)?;
    dict.set_item("port_start", record.port_start)?;
    dict.set_item("port_end", record.port_end)?;
    dict.set_item("database", &record.database)?;
    dict.set_item("env_file", &record.env_file)?;
    dict.set_item("claimed_at", &record.claimed_at)?;
    Ok(dict.unbind())
}

/// Create a stack at the given path and return its port pool.
///
/// The pool arguments carry the same defaults the command line
/// carries, the defaults the library records as DEFAULT_PORT_START
/// and DEFAULT_RANGE_SIZE and DEFAULT_RANGE_COUNT.
#[pyfunction]
#[pyo3(signature = (stack, port_start = 20000, range_size = 10, range_count = 16))]
fn init(
    py: Python<'_>,
    stack: PathBuf,
    port_start: i128,
    range_size: i128,
    range_count: i128,
) -> PyResult<Py<PyDict>> {
    let pool = py
        .detach(|| switchyard_core::init_stack(&stack, port_start, range_size, range_count))
        .map_err(to_py_error)?;
    let dict = PyDict::new(py);
    dict.set_item("port_start", pool.start)?;
    dict.set_item("range_size", pool.range_size)?;
    dict.set_item("range_count", pool.range_count)?;
    Ok(dict.unbind())
}

/// Allocate a worktree, a port range, a database, and an env file
/// for one agent in the stack at the given path.
#[pyfunction]
fn claim(py: Python<'_>, stack: PathBuf, agent: &str) -> PyResult<Py<PyDict>> {
    let record = py
        .detach(|| switchyard_core::claim(&stack, agent))
        .map_err(to_py_error)?;
    record_dict(py, &record)
}

/// Return every resource the agent holds in the stack at the
/// given path and remove its record.
#[pyfunction]
fn release(py: Python<'_>, stack: PathBuf, agent: &str) -> PyResult<Py<PyDict>> {
    let record = py
        .detach(|| switchyard_core::release(&stack, agent))
        .map_err(to_py_error)?;
    record_dict(py, &record)
}

/// List the recorded claims of the stack at the given path,
/// derived only from the registry.
#[pyfunction(name = "list")]
fn list_claims(py: Python<'_>, stack: PathBuf) -> PyResult<Vec<Py<PyDict>>> {
    let records = py
        .detach(|| switchyard_core::list_claims(&stack))
        .map_err(to_py_error)?;
    records
        .iter()
        .map(|record| record_dict(py, record))
        .collect()
}

#[pymodule]
fn _switchyard(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("SwitchyardError", m.py().get_type::<SwitchyardError>())?;
    m.add("RefusalError", m.py().get_type::<RefusalError>())?;
    m.add(
        "NotInitialisedError",
        m.py().get_type::<NotInitialisedError>(),
    )?;
    m.add(
        "CorruptRegistryError",
        m.py().get_type::<CorruptRegistryError>(),
    )?;
    m.add("InvalidNameError", m.py().get_type::<InvalidNameError>())?;
    m.add_function(wrap_pyfunction!(init, m)?)?;
    m.add_function(wrap_pyfunction!(claim, m)?)?;
    m.add_function(wrap_pyfunction!(release, m)?)?;
    m.add_function(wrap_pyfunction!(list_claims, m)?)?;
    Ok(())
}
