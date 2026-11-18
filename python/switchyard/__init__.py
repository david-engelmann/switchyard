"""switchyard, a worktree and port orchestrator with a Rust core.

This package is the thin Python face of the switchyard library.
Each function calls the library and returns the facts the command
line prints as plain dictionaries, with list and queue returning
one dictionary for each standing claim or wait. A refusal from
the library raises a subclass of SwitchyardError carrying the
same message text the command line prints. RefusalError covers
the plain refusals, NotInitialisedError an uninitialised stack,
CorruptRegistryError a corrupt registry, and InvalidNameError an
agent name the library rejects, mirroring the exit classes the
command line reports. An argument of the wrong shape raises
TypeError instead, a boolean where an integer stands, a value of
a wrong type, or a value passed by position for an argument that
stands keyword-only.
"""

from ._switchyard import (
    CorruptRegistryError,
    InvalidNameError,
    NotInitialisedError,
    RefusalError,
    SwitchyardError,
    claim,
    claim_profile,
    claim_routed,
    init,
    list,
    queue,
    release,
)

__all__ = [
    "CorruptRegistryError",
    "InvalidNameError",
    "NotInitialisedError",
    "RefusalError",
    "SwitchyardError",
    "claim",
    "claim_profile",
    "claim_routed",
    "init",
    "list",
    "queue",
    "release",
]

__version__ = "0.1.0"
