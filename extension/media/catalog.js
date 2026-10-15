const DtmCatalog = (() => {
  const collator = new Intl.Collator("ja", { sensitivity: "base" });
  const pathKey = (value) => String(value || "").replace(/\\/g, "/").toLowerCase();
  const key = (config, name) => JSON.stringify([pathKey(config), name]);
  const byName = (a, b) => collator.compare(a.name, b.name) || pathKey(a.configPath).localeCompare(pathKey(b.configPath));
  const hasSource = (inst) => {
    const provider = inst && inst.provider || "";
    return /^(github:|gitbuild:)/.test(provider)
      || /^pkg:(npm|uv|uv-tool):/.test(provider) && !/^pkg:[^:]+:(github:|git\+|https?:)/.test(provider);
  };
  const labels = {
    A: "A：更新元未設定／更新確認未対応",
    B: "B：URL接続型・サーバー本体は管理対象外",
    C: "C：Devinカタログ導入・更新はDTM対象外",
  };
  function build(state) {
    const installations = state.installations || [];
    const managed = state.managed || [];
    const declaredCatalog = new Set((state.catalogEntries || []).map((entry) => key(entry.configPath, entry.serverName)));
    const seen = new Set();
    const used = new Set();
    const rows = [];
    for (const scan of state.scan || []) {
      const rowKey = key(scan.configPath, scan.name);
      if (seen.has(rowKey)) continue;
      seen.add(rowKey);
      const launch = managed.find((entry) => key(entry.configPath, entry.serverName) === rowKey);
      const inst = installations.find((item) => (item.usages || []).some((usage) => key(usage.configPath, usage.serverName) === rowKey))
        || launch && installations.find((item) => item.id === launch.installationId);
      if (inst) used.add(inst.id);
      const catalogSource = scan.registry ? "config" : declaredCatalog.has(rowKey) ? "user" : null;
      const category = catalogSource ? "C" : scan.kind === "McpRemote" ? "B" : hasSource(inst) ? null : "A";
      rows.push({ key: rowKey, name: scan.name, configPath: scan.configPath, scan, inst, launch, category, catalogSource, orphan: false });
    }
    for (const inst of installations.filter((item) => (item.kind || "").startsWith("Mcp") && !used.has(item.id))) {
      rows.push({ key: "orphan:" + inst.id, name: inst.name, inst, category: hasSource(inst) ? null : "A", orphan: true });
    }
    return {
      managed: rows.filter((row) => !row.category).sort(byName),
      unmanaged: rows.filter((row) => row.category).sort((a, b) => a.category.localeCompare(b.category) || byName(a, b)),
      cli: installations.filter((inst) => inst.kind === "CliTool").sort(byName),
    };
  }
  function protectedIds(state) {
    return new Set(build(state).unmanaged.filter((row) => row.inst && /[BC]/.test(row.category)).map((row) => row.inst.id));
  }
  function updateIds(state) {
    const groups = build(state);
    const excluded = protectedIds(state);
    return [...new Set(groups.managed.filter((row) => row.inst && !excluded.has(row.inst.id)).map((row) => row.inst.id)
      .concat(groups.cli.filter((inst) => !excluded.has(inst.id)).map((inst) => inst.id)))];
  }
  return { build, key, pathKey, labels, hasSource, protectedIds, updateIds };
})();
if (typeof module !== "undefined") module.exports = DtmCatalog;
