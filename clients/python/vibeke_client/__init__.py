"""Typed client for the Vibeke control API (``vibeke/1``) over the local socket. Standard library
only; Python 3.11+."""

from .client import (
    MAX_LINE_BYTES,
    Client,
    Event,
    EventOverflow,
    EventStream,
    SocketTrustError,
    VibekeError,
    check_socket_trust,
    default_socket_path,
    runtime_root,
)
from .types_gen import API_VERSION, ERROR_KINDS, METHODS

__all__ = [
    "API_VERSION",
    "ERROR_KINDS",
    "METHODS",
    "MAX_LINE_BYTES",
    "Client",
    "Event",
    "EventOverflow",
    "EventStream",
    "SocketTrustError",
    "VibekeError",
    "check_socket_trust",
    "default_socket_path",
    "runtime_root",
]
