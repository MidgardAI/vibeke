"""Network rules of the dependency proxy of the real Herdr plugin harness (spec 18 §5, §7.2).

The plugin can reach only the proxy, so the proxy is the network boundary. These pure functions
decide which requests the proxy forwards and which upstream addresses it may connect to.
`verified_proxy.py` calls them; `test_hp_netfilter.py` tests them without any network access:

    python3 -m unittest discover -s scripts/herdr-plugins
"""

import ipaddress
import re

ALLOWED_METHODS = frozenset({"GET", "HEAD"})
ALLOWED_PORTS = frozenset({80, 443})

# Addresses the proxy never connects to: anything that is not the public internet. The list is
# explicit because `ipaddress` flags differ between Python versions (for example, multicast
# addresses count as global in some of them).
_BLOCKED_V4 = [
    ipaddress.ip_network(n)
    for n in (
        "0.0.0.0/8",  # "this network" and the unspecified address
        "10.0.0.0/8",  # RFC 1918
        "100.64.0.0/10",  # carrier-grade NAT
        "127.0.0.0/8",  # loopback
        "169.254.0.0/16",  # link-local, including the 169.254.169.254 metadata service
        "172.16.0.0/12",  # RFC 1918
        "192.0.0.0/24",  # IETF protocol assignments
        "192.0.2.0/24",  # documentation
        "192.88.99.0/24",  # 6to4 relay anycast
        "192.168.0.0/16",  # RFC 1918
        "198.18.0.0/15",  # benchmarking
        "198.51.100.0/24",  # documentation
        "203.0.113.0/24",  # documentation
        "224.0.0.0/4",  # multicast
        "240.0.0.0/4",  # reserved, including the broadcast address
    )
]
_BLOCKED_V6 = [
    ipaddress.ip_network(n)
    for n in (
        "::/128",  # unspecified
        "::1/128",  # loopback
        "::ffff:0:0/96",  # IPv4-mapped; checked again as IPv4 below
        "64:ff9b::/96",  # NAT64, which could reach any IPv4 address
        "64:ff9b:1::/48",  # local NAT64
        "100::/64",  # discard
        "2001::/32",  # Teredo
        "2001:db8::/32",  # documentation
        "2002::/16",  # 6to4, which embeds any IPv4 address
        "fc00::/7",  # unique local, including the fd00:ec2::254 metadata service
        "fe80::/10",  # link-local
        "fec0::/10",  # old site-local
        "ff00::/8",  # multicast
    )
]


def address_refusal(address):
    """Why the proxy must not connect to `address` (an IP address string), or None if it may."""
    try:
        ip = ipaddress.ip_address(address.split("%", 1)[0] if isinstance(address, str) else address)
    except ValueError:
        return "not an IP address"
    if isinstance(ip, ipaddress.IPv6Address) and ip.ipv4_mapped is not None:
        inner = address_refusal(str(ip.ipv4_mapped))
        return inner or "IPv4-mapped address"
    blocked = _BLOCKED_V4 if ip.version == 4 else _BLOCKED_V6
    for net in blocked:
        if ip in net:
            return f"address {ip} is in the non-public range {net}"
    if ip.is_multicast or ip.is_loopback or ip.is_link_local or ip.is_unspecified or ip.is_reserved:
        return f"address {ip} is not public"
    if not ip.is_global:
        return f"address {ip} is not a global address"
    return None


def port_refusal(port):
    """Why the proxy must not connect to `port`, or None if it may."""
    if port not in ALLOWED_PORTS:
        return f"port {port} is not 80 or 443"
    return None


def resolved_target(addresses, port):
    """Picks the address to connect to from all addresses a host name resolved to.

    Returns `(address, None)` or `(None, reason)`. A name with any refused address is refused as
    a whole, so a name that mixes public and private addresses cannot reach the private one.
    The caller connects to the returned address itself, so a second DNS answer cannot change it.
    """
    why = port_refusal(port)
    if why:
        return None, why
    unique = []
    for a in addresses:
        if a not in unique:
            unique.append(a)
    if not unique:
        return None, "the host name has no address"
    for a in unique:
        why = address_refusal(a)
        if why:
            return None, why
    return unique[0], None


def request_refusal(mode, method, url, allowed_urls):
    """Why the proxy must refuse an HTTP request before forwarding it, or None.

    `record` and `verify` allow only GET and HEAD. Verify mode also allows only URLs listed in the
    lock; the response hash check stays as a second check. `open` (an `unpinned` plugin that
    reads the GitHub API) forwards every method: the plugin could reach the network without the
    proxy too, and the proxy is there only to add the token (`github_token_allowed`).
    """
    if mode == "open":
        return None
    if method not in ALLOWED_METHODS:
        return f"method {method} is not GET or HEAD"
    if mode == "verify" and url not in allowed_urls:
        return "URL is not in the lock"
    return None


GITHUB_API_HOST = "api.github.com"

_REPOSITORY = re.compile(r"[A-Za-z0-9][A-Za-z0-9-]*/(?!\.\.?\Z)[A-Za-z0-9_.-]+")
# A tag, branch or commit name in one path segment. Dots are allowed (v1.2.3), but not a segment
# of dots only.
_REF = r"(?!\.+(?:/|\Z))[A-Za-z0-9._-]{1,100}"
# The API paths below `/repos/<owner>/<repo>` that the locked plugins read to find a release.
_REPO_PATHS = re.compile(
    r"(?:"
    r"|/commits/" + _REF
    + r"|/releases|/releases/latest|/releases/tags/" + _REF
    + r"|/releases/[0-9]{1,20}|/releases/assets/[0-9]{1,20}"
    + r"|/tags|/git/ref/tags/" + _REF + r"|/git/refs/tags/" + _REF
    + r")"
)
_QUERY = re.compile(r"(?:per_page|page)=[0-9]{1,4}(?:&(?:per_page|page)=[0-9]{1,4})?")


def valid_repository(name):
    """Whether `name` is an `owner/repo` GitHub repository name."""
    return isinstance(name, str) and _REPOSITORY.fullmatch(name) is not None


def github_token_allowed(method, scheme, host, host_header, port, path, repositories):
    """Whether the proxy may add its GitHub token to this request.

    Only GET and HEAD over HTTPS on port 443 to `api.github.com` (the connection's host and the
    Host header), for the paths in `_REPO_PATHS` below `/repos/<owner>/<repo>` of one of
    `repositories` (the plugin's own repository from the lock). The path must be plain: no
    percent-encoding, no empty or dot segments, and a query of at most `per_page` and `page`.
    Owner and repository names compare without case, as GitHub does.
    """
    if method not in ALLOWED_METHODS or scheme != "https" or port != 443:
        return False
    if not isinstance(host, str) or host.lower() != GITHUB_API_HOST:
        return False
    if host_header is not None and host_header.lower().split(":", 1)[0] != GITHUB_API_HOST:
        return False
    if not isinstance(path, str) or not path.startswith("/repos/"):
        return False
    path, _, query = path.partition("?")
    if query and not _QUERY.fullmatch(query):
        return False
    if "%" in path or "//" in path or "\\" in path or not path.isascii():
        return False
    segments = path.split("/")[1:]
    if any(seg in ("", ".", "..") for seg in segments):
        return False
    rest = path[len("/repos/"):]
    for repo in repositories:
        if not valid_repository(repo):
            continue
        prefix = repo.lower()
        if rest.lower() == prefix or rest.lower().startswith(prefix + "/"):
            if _REPO_PATHS.fullmatch(rest[len(prefix):]):
                return True
    return False


# Visible ASCII only: no spaces, controls or non-ASCII, so the token cannot end the header or
# add another one. The format of GitHub's tokens is not fixed (base64 characters such as `+`,
# `/` and `=` can appear), so the check does not narrow it further.
_TOKEN = re.compile(r"[\x21-\x7e]{1,4096}")


def valid_token(token):
    """Whether `token` can go into an Authorization header unchanged (no spaces or controls)."""
    return isinstance(token, str) and _TOKEN.fullmatch(token) is not None
