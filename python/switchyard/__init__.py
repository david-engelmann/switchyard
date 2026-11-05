"""switchyard, a worktree and port orchestrator with a Rust core.

This package is the thin Python face of the switchyard library.
Each function calls the library and returns plain dictionaries of
the facts the command line prints, and a refusal raises a subclass
of SwitchyardError carrying the same message text the command line
prints. RefusalError covers the plain refusals, NotInitialisedError
an uninitialised stack, CorruptRegistryError a corrupt registry,
and InvalidNameError an agent name the library rejects, mirroring
the exit classes the command line reports.
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
    "release",
]

__version__ = "0.1.0"
