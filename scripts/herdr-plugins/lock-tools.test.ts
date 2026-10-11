import { describe, expect, test } from "bun:test";
import { merge, parseRecords } from "../herdr-plugins-record";
import { allowlist, deferredPlugins, matrix, parseLock } from "./lock-tools";

const LOCK = `schema = 1
baseline = "0.9.3"
catalog = "catalog.json"
catalog_sha256 = "${"a".repeat(64)}"
catalog_generated_at = "2026-10-11T10:00:42.514Z"
size = 3

[toolchains]
node = "22.20.0"

[[exclude]]
id = 1
reason = "covered elsewhere"

[[plugin]]
rank = 1
repository = "acme/multi"
repo_id = 10
plugin_id = "acme.one"
manifest_path = "one/herdr-plugin.toml"
commit = "${"1".repeat(40)}"
manifest_sha256 = "${"b".repeat(64)}"
stars = 30
created_at = "2026-07-06T16:31:11Z"
platforms = ["linux"]
dependency_mode = "unpinned"

[[plugin]]
rank = 2
repository = "acme/multi"
repo_id = 10
plugin_id = "acme.two"
manifest_path = "two/herdr-plugin.toml"
commit = "${"1".repeat(40)}"
manifest_sha256 = "${"c".repeat(64)}"
stars = 30
created_at = "2026-07-06T16:31:11Z"
dependency_mode = "verified"

[[plugin.dependency]]
url = "https://example.com/old"
sha256 = "${"d".repeat(64)}"

[[plugin]]
rank = 3
repository = "solo/tool"
repo_id = 11
plugin_id = "solo.tool"
manifest_path = "herdr-plugin.toml"
commit = "${"2".repeat(40)}"
manifest_sha256 = "${"e".repeat(64)}"
stars = 20
created_at = "2026-07-07T16:31:11Z"
platforms = ["macos", "linux"]
dependency_mode = "unpinned"
`;

describe("lock tools", () => {
  const lock = parseLock(LOCK);

  test("matrix has one entry per supported platform with stems and rank filters", () => {
    const m = matrix(lock, "", ["linux", "macos"]);
    expect(m.map((e) => `${e.stem}-${e.os}`)).toEqual([
      "acme__multi__acme.one-linux",
      "acme__multi__acme.two-linux",
      "acme__multi__acme.two-macos",
      "solo__tool-linux",
      "solo__tool-macos",
    ]);
    expect(m[0].filter).toBe("test(/^compat_herdr_real_plugins::plugin_01_/)");
    expect(m[0].runner).toBe("ubuntu-latest");
  });

  test("the filter selects by plugin id, repository or rank", () => {
    expect(matrix(lock, "acme.one", ["linux"]).map((e) => e.plugin_id)).toEqual(["acme.one"]);
    expect(matrix(lock, "solo/tool, 1", ["linux"]).map((e) => e.plugin_id)).toEqual(["acme.one", "solo.tool"]);
    expect(matrix(lock, "nothing", ["linux"])).toEqual([]);
  });

  test("deferred plugins keep their rank but leave the matrix", () => {
    const deferred = parseLock(
      LOCK.replace("[[exclude]]", '[[defer]]\nrepository = "acme/multi"\nreason = "Needs an account."\n\n[[exclude]]'),
    );
    expect(matrix(deferred, "", ["linux", "macos"]).map((e) => e.plugin_id)).toEqual(["solo.tool", "solo.tool"]);
    // Asking for it by name does not bring it back.
    expect(matrix(deferred, "acme.one", ["linux"])).toEqual([]);
    expect(deferredPlugins(deferred)).toEqual([
      { rank: 1, plugin_id: "acme.one", repository: "acme/multi", reason: "Needs an account." },
      { rank: 2, plugin_id: "acme.two", repository: "acme/multi", reason: "Needs an account." },
    ]);
    expect(deferredPlugins(lock)).toEqual([]);
  });

  test("allowlist lists the locked dependencies", () => {
    expect(allowlist(lock, "acme.two").dependencies).toEqual([{ url: "https://example.com/old", sha256: "d".repeat(64) }]);
    expect(() => allowlist(lock, "missing")).toThrow();
  });
});

describe("record merge", () => {
  const lock = parseLock(LOCK);
  const deps = parseRecords([
    `{"url":"https://b.example/x","sha256":"${"f".repeat(64)}"}\n{"url":"https://a.example/y","sha256":"${"0".repeat(64)}"}\n`,
    `{"url":"https://b.example/x","sha256":"${"f".repeat(64)}","status":200}\n`,
  ]);

  test("records are deduplicated and sorted", () => {
    expect(deps.map((d) => d.url)).toEqual(["https://a.example/y", "https://b.example/x"]);
  });

  test("bad records are refused", () => {
    expect(() => parseRecords(['{"url":"ftp://x","sha256":"00"}'])).toThrow();
    expect(() => parseRecords([`{"url":"https://x/\\"","sha256":"${"0".repeat(64)}"}`])).toThrow();
  });

  test("merging switches the plugin to verified and replaces its dependencies", () => {
    for (const id of ["acme.one", "acme.two", "solo.tool"]) {
      const next = parseLock(merge(LOCK, id, deps));
      const p = next.plugin!.find((q) => q.plugin_id === id)!;
      expect(p.dependency_mode).toBe("verified");
      expect(p.dependency).toEqual(deps);
      for (const other of next.plugin!.filter((q) => q.plugin_id !== id)) {
        expect(other).toEqual(lock.plugin!.find((q) => q.plugin_id === other.plugin_id)!);
      }
    }
    expect(() => merge(LOCK, "missing", deps)).toThrow();
  });
});
