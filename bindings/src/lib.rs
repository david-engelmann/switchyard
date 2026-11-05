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
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict};

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

/// Resolve one pool argument of init.
///
/// An argument the caller leaves out resolves to the default the
/// library records for it, read from the library constant at call
/// time, so the library stays the single source of truth for its
/// own defaults. A boolean is refused outright with a TypeError
/// naming the argument, because Python spells True and False as
/// the integers 1 and 0 and a pool argument is never meant as a
/// truth value.
fn pool_argument(value: Option<&Bound<'_, PyAny>>, name: &str, default: i64) -> PyResult<i128> {
    match value {
        None => Ok(i128::from(default)),
        Some(value) if value.is_instance_of::<PyBool>() => Err(PyTypeError::new_err(format!(
            "{name} must be an integer, not a boolean"
        ))),
        Some(value) => value.extract::<i128>(),
    }
}

/// Create a stack at the given path and return its port pool.
///
/// The pool arguments default to the values the library records
/// as DEFAULT_PORT_START and DEFAULT_RANGE_SIZE and
/// DEFAULT_RANGE_COUNT, resolved from the library itself.
#[pyfunction]
#[pyo3(signature = (stack, port_start = None, range_size = None, range_count = None))]
fn init(
    py: Python<'_>,
    stack: PathBuf,
    port_start: Option<Bound<'_, PyAny>>,
    range_size: Option<Bound<'_, PyAny>>,
    range_count: Option<Bound<'_, PyAny>>,
) -> PyResult<Py<PyDict>> {
    let port_start = pool_argument(
        port_start.as_ref(),
        "port_start",
        switchyard_core::DEFAULT_PORT_START,
    )?;
    let range_size = pool_argument(
        range_size.as_ref(),
        "range_size",
        switchyard_core::DEFAULT_RANGE_SIZE,
    )?;
    let range_count = pool_argument(
        range_count.as_ref(),
        "range_count",
        switchyard_core::DEFAULT_RANGE_COUNT,
    )?;
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

/// Allocate a claim under a named declared profile, with no
/// consultation at all, by the same path a plain claim takes with
/// the draw of the profile. An unknown profile name is refused
/// with the message the command line prints for it.
#[pyfunction]
fn claim_profile(
    py: Python<'_>,
    stack: PathBuf,
    agent: &str,
    profile: &str,
) -> PyResult<Py<PyDict>> {
    let profile = switchyard_core::profile_named(profile).map_err(to_py_error)?;
    let record = py
        .detach(|| switchyard_core::claim_profile(&stack, agent, profile))
        .map_err(to_py_error)?;
    record_dict(py, &record)
}

/// The binding side of the transport seam, a Python callable
/// standing behind the library trait.
///
/// The callable receives the exact request document and answers
/// with the reply body as text. A callable that raises, and a
/// callable that returns a value that is not text, each surface
/// as a transport failure of theirs, so the claim lands on its
/// declared default under the transport class and the routing log
/// records the landing.
struct CallableTransport {
    callable: Py<PyAny>,
}

impl switchyard_core::JudgeTransport for CallableTransport {
    fn consult(&self, request: &str) -> Result<String, String> {
        Python::attach(|py| {
            let reply = self
                .callable
                .call1(py, (request,))
                .map_err(|error| format!("the transport callable raised, {error}"))?;
            reply
                .extract::<String>(py)
                .map_err(|_| "the transport callable returned a value that is not text".to_string())
        })
    }
}

/// Allocate a claim whose profile a model settles from a job
/// description. The transport argument is a callable standing
/// behind the library seam, and leaving it out configures no
/// transport at all, so the claim lands on its declared default
/// under the no transport class exactly as a routed claim from the
/// command line with no judge command does.
#[pyfunction]
#[pyo3(signature = (stack, agent, description, default_profile, transport = None))]
fn claim_routed(
    py: Python<'_>,
    stack: PathBuf,
    agent: &str,
    description: &str,
    default_profile: &str,
    transport: Option<Py<PyAny>>,
) -> PyResult<Py<PyDict>> {
    let default = switchyard_core::profile_named(default_profile).map_err(to_py_error)?;
    let transport = transport.map(|callable| CallableTransport { callable });
    let record = py
        .detach(|| {
            switchyard_core::claim_routed(
                &stack,
                agent,
                description,
                default,
                transport
                    .as_ref()
                    .map(|transport| transport as &dyn switchyard_core::JudgeTransport),
            )
        })
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
    m.add_function(wrap_pyfunction!(claim_profile, m)?)?;
    m.add_function(wrap_pyfunction!(claim_routed, m)?)?;
    m.add_function(wrap_pyfunction!(release, m)?)?;
    m.add_function(wrap_pyfunction!(list_claims, m)?)?;
    Ok(())
}
