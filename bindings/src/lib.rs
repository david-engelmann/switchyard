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
use pyo3::exceptions::{PyOverflowError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyList};

use switchyard_core::{
    ClaimOutcome, DoctorReport, Error, InitOptions, Record, ReleaseOptions, ReleaseOutcome, Wait,
};

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

/// Translate one recorded wait into a plain dictionary carrying
/// every field the wait record of obligation SY-18 carries.
fn wait_dict(py: Python<'_>, wait: &Wait) -> PyResult<Py<PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("agent", &wait.agent)?;
    dict.set_item("profile", &wait.profile)?;
    dict.set_item("recorded_at", &wait.recorded_at)?;
    dict.set_item("wait_for", wait.wait_for)?;
    dict.set_item("score", wait.score)?;
    dict.set_item("confidence", wait.confidence)?;
    dict.set_item("model", &wait.model)?;
    dict.set_item("fallback", &wait.fallback)?;
    Ok(dict.unbind())
}

/// Translate the outcome of a waiting claim into the dictionary
/// of the record it produced, the claim record where it
/// allocated and the wait record where it waited, under
/// obligation SY-24.
fn outcome_dict(py: Python<'_>, outcome: &ClaimOutcome) -> PyResult<Py<PyDict>> {
    match outcome {
        ClaimOutcome::Claim(record) => record_dict(py, record),
        ClaimOutcome::Wait(wait) => wait_dict(py, wait),
    }
}

/// Map the failure of an integer extraction to the refusal the
/// binding owes where the failure is the value's width.
///
/// Pyo3 raises a bare OverflowError for a Python integer outside
/// the width the argument must fit, and that error is not a
/// SwitchyardError and carries no refusal text, so left unmapped
/// it escapes the promise this file's header makes. Where the
/// failure is the width, the refusal names the argument and the
/// width it must fit. Every other extraction failure passes
/// through as it stands.
fn extraction_refusal(error: PyErr, py: Python<'_>, name: &str, width: u32) -> PyErr {
    if error.is_instance_of::<PyOverflowError>(py) {
        RefusalError::new_err(format!("{name} must fit in a {width}-bit integer"))
    } else {
        error
    }
}

/// Resolve the wait bound of a waiting claim.
///
/// A boolean is refused outright with a TypeError naming the
/// argument, the way a pool argument is refused, and a bound
/// named without the wait option is refused the way the command
/// line refuses it, because a bound is meaningful only beside a
/// claim that waits. The bound itself stands unset by default,
/// and its positivity is the library's refusal to make. A bound
/// too wide for the count of seconds it names is refused by name
/// through the extraction mapping above.
fn wait_bound(wait: bool, wait_for: Option<&Bound<'_, PyAny>>) -> PyResult<Option<i64>> {
    match wait_for {
        None => Ok(None),
        Some(value) if value.is_instance_of::<PyBool>() => Err(PyTypeError::new_err(
            "wait_for must be an integer, not a boolean",
        )),
        Some(value) => {
            if !wait {
                return Err(RefusalError::new_err("wait_for stands only beside wait"));
            }
            let bound = value
                .extract::<i64>()
                .map_err(|error| extraction_refusal(error, value.py(), "wait_for", 64))?;
            Ok(Some(bound))
        }
    }
}

/// Resolve one pool argument of init.
///
/// An argument the caller leaves out resolves to the default the
/// library records for it, read from the library constant at call
/// time, so the library stays the single source of truth for its
/// own defaults. A boolean is refused outright with a TypeError
/// naming the argument, because Python spells True and False as
/// the integers 1 and 0 and a pool argument is never meant as a
/// truth value. An integer too wide for the argument is refused
/// by name through the extraction mapping, naming the argument
/// and the width it must fit.
fn pool_argument(value: Option<&Bound<'_, PyAny>>, name: &str, default: i64) -> PyResult<i128> {
    match value {
        None => Ok(i128::from(default)),
        Some(value) if value.is_instance_of::<PyBool>() => Err(PyTypeError::new_err(format!(
            "{name} must be an integer, not a boolean"
        ))),
        Some(value) => value
            .extract::<i128>()
            .map_err(|error| extraction_refusal(error, value.py(), name, 128)),
    }
}

/// Create a stack at the given path and return its port pool.
///
/// The pool arguments default to the values the library records
/// as DEFAULT_PORT_START and DEFAULT_RANGE_SIZE and
/// DEFAULT_RANGE_COUNT, resolved from the library itself. The
/// pool arguments stand keyword-only, so no caller can place
/// one in another's slot by position.
#[pyfunction]
#[pyo3(signature = (stack, *, port_start = None, range_size = None, range_count = None, from_repo = None, branch = None))]
fn init(
    py: Python<'_>,
    stack: PathBuf,
    port_start: Option<Bound<'_, PyAny>>,
    range_size: Option<Bound<'_, PyAny>>,
    range_count: Option<Bound<'_, PyAny>>,
    from_repo: Option<PathBuf>,
    branch: Option<String>,
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
    let options = InitOptions { from_repo, branch };
    let pool = py
        .detach(|| {
            switchyard_core::init_stack_with_options(
                &stack,
                port_start,
                range_size,
                range_count,
                &options,
            )
        })
        .map_err(to_py_error)?;
    let dict = PyDict::new(py);
    dict.set_item("port_start", pool.start)?;
    dict.set_item("range_size", pool.range_size)?;
    dict.set_item("range_count", pool.range_count)?;
    Ok(dict.unbind())
}

/// Allocate a worktree, a port range, a database, and an env file
/// for one agent in the stack at the given path. With the wait
/// option standing, the claim waits exactly as a command line
/// claim with --wait waits and returns the wait record where it
/// waited, and the transport callable is consulted for the
/// scoring question only where the claim waits. A transport
/// supplied beside a claim that does not wait is refused by
/// name, mirroring the command line's refusal of a judge command
/// beside a claim that neither describes nor waits. The optional
/// arguments stand keyword-only, as in every claim function of
/// this binding, so no caller can place one in another's slot.
#[pyfunction]
#[pyo3(signature = (stack, agent, *, wait = false, wait_for = None, transport = None))]
fn claim(
    py: Python<'_>,
    stack: PathBuf,
    agent: &str,
    wait: bool,
    wait_for: Option<Bound<'_, PyAny>>,
    transport: Option<Py<PyAny>>,
) -> PyResult<Py<PyDict>> {
    let bound = wait_bound(wait, wait_for.as_ref())?;
    if !wait && transport.is_some() {
        return Err(RefusalError::new_err("transport stands only beside wait"));
    }
    if !wait {
        let record = py
            .detach(|| switchyard_core::claim(&stack, agent))
            .map_err(to_py_error)?;
        return record_dict(py, &record);
    }
    let transport = wrapped_transport(transport);
    let outcome = py
        .detach(|| switchyard_core::claim_waiting(&stack, agent, bound, lent_transport(&transport)))
        .map_err(to_py_error)?;
    outcome_dict(py, &outcome)
}

/// Allocate a claim under a named declared profile, with no
/// consultation at all, by the same path a plain claim takes with
/// the draw of the profile. An unknown profile name is refused
/// with the message the command line prints for it. With the wait
/// option standing, the claim waits exactly as a command line
/// claim with --wait waits. A transport supplied beside a claim
/// that does not wait is refused by name, as in claim. The
/// optional arguments stand keyword-only, as in claim.
#[pyfunction]
#[pyo3(signature = (stack, agent, profile, *, wait = false, wait_for = None, transport = None))]
fn claim_profile(
    py: Python<'_>,
    stack: PathBuf,
    agent: &str,
    profile: &str,
    wait: bool,
    wait_for: Option<Bound<'_, PyAny>>,
    transport: Option<Py<PyAny>>,
) -> PyResult<Py<PyDict>> {
    let bound = wait_bound(wait, wait_for.as_ref())?;
    if !wait && transport.is_some() {
        return Err(RefusalError::new_err("transport stands only beside wait"));
    }
    let profile = switchyard_core::profile_named(profile).map_err(to_py_error)?;
    if !wait {
        let record = py
            .detach(|| switchyard_core::claim_profile(&stack, agent, profile))
            .map_err(to_py_error)?;
        return record_dict(py, &record);
    }
    let transport = wrapped_transport(transport);
    let outcome = py
        .detach(|| {
            switchyard_core::claim_profile_waiting(
                &stack,
                agent,
                profile,
                bound,
                lent_transport(&transport),
            )
        })
        .map_err(to_py_error)?;
    outcome_dict(py, &outcome)
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

/// Build the transport that stands behind the library seam from
/// the callable a claim function was handed, the one wrapping
/// every claim function in this binding uses.
fn wrapped_transport(transport: Option<Py<PyAny>>) -> Option<CallableTransport> {
    transport.map(|callable| CallableTransport { callable })
}

/// Lend a built transport to a library call as the trait object,
/// the one lending every claim function in this binding uses.
fn lent_transport(
    transport: &Option<CallableTransport>,
) -> Option<&dyn switchyard_core::JudgeTransport> {
    transport
        .as_ref()
        .map(|transport| transport as &dyn switchyard_core::JudgeTransport)
}

/// Allocate a claim whose profile a model settles from a job
/// description. The transport argument is a callable standing
/// behind the library seam, and leaving it out configures no
/// transport at all, so the claim lands on its declared default
/// under the no transport class exactly as a routed claim from the
/// command line with no judge command does. With the wait option
/// standing, the claim waits exactly as a command line claim with
/// --wait waits, and the same callable answers the scoring
/// question beside the routing question. The optional arguments
/// stand keyword-only, as in every claim function of this
/// binding, transport included, though it keeps its fifth place.
#[allow(clippy::too_many_arguments)]
#[pyfunction]
#[pyo3(signature = (stack, agent, description, default_profile, *, transport = None, wait = false, wait_for = None))]
fn claim_routed(
    py: Python<'_>,
    stack: PathBuf,
    agent: &str,
    description: &str,
    default_profile: &str,
    transport: Option<Py<PyAny>>,
    wait: bool,
    wait_for: Option<Bound<'_, PyAny>>,
) -> PyResult<Py<PyDict>> {
    let bound = wait_bound(wait, wait_for.as_ref())?;
    let default = switchyard_core::profile_named(default_profile).map_err(to_py_error)?;
    let transport = wrapped_transport(transport);
    if wait {
        let outcome = py
            .detach(|| {
                switchyard_core::claim_routed_waiting(
                    &stack,
                    agent,
                    description,
                    default,
                    bound,
                    lent_transport(&transport),
                )
            })
            .map_err(to_py_error)?;
        return outcome_dict(py, &outcome);
    }
    let record = py
        .detach(|| {
            switchyard_core::claim_routed(
                &stack,
                agent,
                description,
                default,
                lent_transport(&transport),
            )
        })
        .map_err(to_py_error)?;
    record_dict(py, &record)
}

/// Return every resource the agent holds in the stack at the
/// given path and remove its record. Where the agent holds only
/// a wait, the release cancels the wait and returns the wait
/// record, under obligation SY-22. Keyword-only `keep_branch`
/// and `safe` flags expose the non-destructive workflows of
/// obligation SY-28.
#[pyfunction]
#[pyo3(signature = (stack, agent, *, keep_branch = false, safe = false))]
fn release(
    py: Python<'_>,
    stack: PathBuf,
    agent: &str,
    keep_branch: bool,
    safe: bool,
) -> PyResult<Py<PyDict>> {
    let options = ReleaseOptions { keep_branch, safe };
    let outcome = py
        .detach(|| switchyard_core::release_with_options(&stack, agent, options))
        .map_err(to_py_error)?;
    match &outcome {
        ReleaseOutcome::Claim(record) => record_dict(py, record),
        ReleaseOutcome::Wait(wait) => wait_dict(py, wait),
    }
}

fn doctor_dict(py: Python<'_>, report: &DoctorReport) -> PyResult<Py<PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("ok", report.ok)?;
    dict.set_item("fixed", report.fixed)?;
    let issues = PyList::empty(py);
    for issue in &report.issues {
        let item = PyDict::new(py);
        item.set_item("kind", &issue.kind)?;
        item.set_item("target", &issue.target)?;
        item.set_item("detail", &issue.detail)?;
        item.set_item("fixed", issue.fixed)?;
        issues.append(item)?;
    }
    dict.set_item("issues", issues)?;
    dict.set_item("kept_branches", &report.kept_branches)?;
    Ok(dict.unbind())
}

/// Audit stack health under `switchyard.lock` and optionally
/// repair clean orphan residue when `fix` is True, under
/// obligation SY-29.
#[pyfunction]
#[pyo3(signature = (stack, *, fix = false))]
fn doctor(py: Python<'_>, stack: PathBuf, fix: bool) -> PyResult<Py<PyDict>> {
    let report = py
        .detach(|| switchyard_core::doctor(&stack, fix))
        .map_err(to_py_error)?;
    doctor_dict(py, &report)
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

/// List the standing waits of the stack at the given path in
/// offer order, as plain dictionaries carrying the fields of
/// obligation SY-18, under obligation SY-24.
#[pyfunction(name = "queue")]
fn queue(py: Python<'_>, stack: PathBuf) -> PyResult<Vec<Py<PyDict>>> {
    let waits = py
        .detach(|| switchyard_core::list_waits(&stack))
        .map_err(to_py_error)?;
    waits.iter().map(|wait| wait_dict(py, wait)).collect()
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
    m.add_function(wrap_pyfunction!(doctor, m)?)?;
    m.add_function(wrap_pyfunction!(list_claims, m)?)?;
    m.add_function(wrap_pyfunction!(queue, m)?)?;
    Ok(())
}
