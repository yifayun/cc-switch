import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Server,
  Plus,
  Trash2,
  Loader2,
  Save,
  UploadCloud,
  Cable,
  RefreshCw,
  ChevronDown,
  ChevronRight,
  Info,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { settingsApi } from "@/lib/api";
import type { CodexSshHost, CodexSshSyncSettings } from "@/types";

function newHost(): CodexSshHost {
  return {
    id: crypto.randomUUID(),
    name: "",
    host: "",
    port: 22,
    user: "root",
    identityFile: "",
    sshAlias: "",
    remoteCodexDir: "~/.codex",
    enabled: true,
    autoSync: true,
    syncOnSshConnect: true,
    forwardProxy: true,
  };
}

interface CodexSshSyncSectionProps {
  initial?: CodexSshSyncSettings | null;
  onSaved?: (settings: CodexSshSyncSettings) => void;
}

export function CodexSshSyncSection({
  initial,
  onSaved,
}: CodexSshSyncSectionProps) {
  const { t } = useTranslation();
  const [enabled, setEnabled] = useState(false);
  const [hosts, setHosts] = useState<CodexSshHost[]>([]);
  const [saving, setSaving] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [testingId, setTestingId] = useState<string | null>(null);
  const [expandedIds, setExpandedIds] = useState<Set<string>>(new Set());

  useEffect(() => {
    setEnabled(initial?.enabled ?? false);
    const nextHosts = (initial?.hosts ?? []).map((h) => ({
      ...newHost(),
      ...h,
      port: h.port ?? 22,
    }));
    setHosts(nextHosts);
    // Newly added empty hosts start expanded; existing stay collapsed for quick manage.
    setExpandedIds((prev) => {
      const next = new Set(prev);
      for (const h of nextHosts) {
        if (!h.host?.trim()) next.add(h.id);
      }
      return next;
    });
  }, [initial]);

  const draft = useMemo<CodexSshSyncSettings>(
    () => ({
      enabled,
      hosts: hosts.map((h) => ({
        ...h,
        name: h.name?.trim() || h.host,
        host: h.host.trim(),
        user: h.user.trim(),
        port: h.port && h.port > 0 ? h.port : 22,
        identityFile: h.identityFile?.trim() || undefined,
        sshAlias: h.sshAlias?.trim() || undefined,
        remoteCodexDir: h.remoteCodexDir?.trim() || "~/.codex",
      })),
    }),
    [enabled, hosts],
  );

  const updateHost = useCallback(
    (id: string, patch: Partial<CodexSshHost>) => {
      setHosts((prev) =>
        prev.map((h) => (h.id === id ? { ...h, ...patch } : h)),
      );
    },
    [],
  );

  const toggleExpanded = (id: string) => {
    setExpandedIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const collapseAll = () => setExpandedIds(new Set());
  const expandAll = () => setExpandedIds(new Set(hosts.map((h) => h.id)));

  const handleSave = async () => {
    if (enabled) {
      for (const host of draft.hosts ?? []) {
        if (!host.host?.trim() || !host.user?.trim()) {
          toast.error(t("settings.codexSshSync.hostRequired"));
          return;
        }
      }
    }
    setSaving(true);
    try {
      const result = await settingsApi.codexSshSyncSaveSettings(draft);
      onSaved?.(result.settings);
      const lastError = result.settings.hosts?.find((h) => h.lastError)?.lastError;
      if (lastError) {
        toast.warning(
          t("settings.codexSshSync.saveOkSyncFailed", { error: lastError }),
        );
      } else {
        toast.success(t("settings.codexSshSync.saveSuccess"));
      }
    } catch (error) {
      toast.error(
        t("settings.codexSshSync.saveFailed", {
          error: error instanceof Error ? error.message : String(error),
        }),
      );
    } finally {
      setSaving(false);
    }
  };

  const handleSyncNow = async (hostId?: string) => {
    setSyncing(true);
    try {
      await settingsApi.codexSshSyncSaveSettings(draft);
      const result = await settingsApi.codexSshSyncNow(hostId);
      const failed = result.results.filter((r) => !r.success);
      if (failed.length === 0 && result.results.length > 0) {
        toast.success(t("settings.codexSshSync.syncSuccess"));
      } else if (failed.length > 0) {
        toast.error(
          t("settings.codexSshSync.syncPartial", {
            error: failed.map((f) => f.message).join("; "),
          }),
        );
      } else {
        toast.info(t("settings.codexSshSync.syncEmpty"));
      }
      const refreshed = await settingsApi.codexSshSyncGetSettings();
      if (refreshed) onSaved?.(refreshed);
    } catch (error) {
      toast.error(
        t("settings.codexSshSync.syncFailed", {
          error: error instanceof Error ? error.message : String(error),
        }),
      );
    } finally {
      setSyncing(false);
    }
  };

  const handleTest = async (host: CodexSshHost) => {
    setTestingId(host.id);
    try {
      await settingsApi.codexSshSyncTestHost(host);
      toast.success(t("settings.codexSshSync.testSuccess"));
    } catch (error) {
      toast.error(
        t("settings.codexSshSync.testFailed", {
          error: error instanceof Error ? error.message : String(error),
        }),
      );
    } finally {
      setTestingId(null);
    }
  };

  const handleAddHost = () => {
    const host = newHost();
    setHosts((prev) => [...prev, host]);
    setExpandedIds((prev) => new Set(prev).add(host.id));
  };

  return (
    <section className="space-y-4">
      <div className="flex items-center gap-2 pb-2 border-b border-border/40">
        <Server className="h-4 w-4 text-primary" />
        <div className="flex-1">
          <h3 className="text-sm font-medium">
            {t("settings.codexSshSync.title")}
          </h3>
          <p className="text-xs text-muted-foreground mt-0.5">
            {t("settings.codexSshSync.description")}
          </p>
        </div>
        <Switch checked={enabled} onCheckedChange={setEnabled} />
      </div>

      {enabled ? (
        <div className="space-y-4">
          <p className="text-xs text-muted-foreground leading-relaxed">
            {t("settings.codexSshSync.hint")}
          </p>
          <div className="flex items-start gap-2 rounded-md border border-sky-500/30 bg-sky-500/5 px-3 py-2 text-xs text-muted-foreground">
            <Info className="h-3.5 w-3.5 mt-0.5 text-sky-500 shrink-0" />
            <p>{t("settings.codexSshSync.deviceControlHint")}</p>
          </div>

          <div className="flex flex-wrap gap-2">
            <Button type="button" variant="outline" size="sm" onClick={collapseAll}>
              {t("settings.codexSshSync.collapseAll")}
            </Button>
            <Button type="button" variant="outline" size="sm" onClick={expandAll}>
              {t("settings.codexSshSync.expandAll")}
            </Button>
          </div>

          {hosts.map((host) => {
            const expanded = expandedIds.has(host.id);
            const title = host.name?.trim() || host.host || t("settings.codexSshSync.namePlaceholder");
            return (
              <div
                key={host.id}
                className="rounded-lg border border-border/60 overflow-hidden"
              >
                <div className="flex items-center gap-2 px-3 py-2 bg-muted/30">
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    className="h-7 w-7"
                    onClick={() => toggleExpanded(host.id)}
                    title={
                      expanded
                        ? t("settings.codexSshSync.collapse")
                        : t("settings.codexSshSync.expand")
                    }
                  >
                    {expanded ? (
                      <ChevronDown className="h-4 w-4" />
                    ) : (
                      <ChevronRight className="h-4 w-4" />
                    )}
                  </Button>
                  <button
                    type="button"
                    className="flex-1 text-left text-sm font-medium truncate"
                    onClick={() => toggleExpanded(host.id)}
                  >
                    {title}
                    {host.host ? (
                      <span className="ml-2 text-xs text-muted-foreground font-normal">
                        {host.user}@{host.host}:{host.port ?? 22}
                      </span>
                    ) : null}
                  </button>
                  <Switch
                    checked={host.enabled ?? true}
                    onCheckedChange={(v) => updateHost(host.id, { enabled: v })}
                  />
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    className="h-7 w-7"
                    disabled={syncing}
                    onClick={() => void handleSyncNow(host.id)}
                    title={t("settings.codexSshSync.syncOne")}
                  >
                    <UploadCloud className="h-3.5 w-3.5" />
                  </Button>
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    className="h-7 w-7"
                    onClick={() =>
                      setHosts((prev) => prev.filter((h) => h.id !== host.id))
                    }
                  >
                    <Trash2 className="h-4 w-4 text-destructive" />
                  </Button>
                </div>

                {expanded ? (
                  <div className="p-4 space-y-3 border-t border-border/50">
                    <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
                      <div className="space-y-1 sm:col-span-2">
                        <label className="text-xs text-muted-foreground">
                          {t("settings.codexSshSync.namePlaceholder")}
                        </label>
                        <Input
                          value={host.name ?? ""}
                          onChange={(e) =>
                            updateHost(host.id, { name: e.target.value })
                          }
                          placeholder={t("settings.codexSshSync.namePlaceholder")}
                        />
                      </div>
                      <div className="space-y-1">
                        <label className="text-xs text-muted-foreground">
                          {t("settings.codexSshSync.host")}
                        </label>
                        <Input
                          value={host.host}
                          onChange={(e) =>
                            updateHost(host.id, { host: e.target.value })
                          }
                          placeholder="154.201.64.118"
                        />
                      </div>
                      <div className="space-y-1">
                        <label className="text-xs text-muted-foreground">
                          {t("settings.codexSshSync.user")}
                        </label>
                        <Input
                          value={host.user}
                          onChange={(e) =>
                            updateHost(host.id, { user: e.target.value })
                          }
                          placeholder="root"
                        />
                      </div>
                      <div className="space-y-1">
                        <label className="text-xs text-muted-foreground">
                          {t("settings.codexSshSync.port")}
                        </label>
                        <Input
                          type="number"
                          value={host.port ?? 22}
                          onChange={(e) =>
                            updateHost(host.id, {
                              port: Number(e.target.value) || 22,
                            })
                          }
                        />
                      </div>
                      <div className="space-y-1">
                        <label className="text-xs text-muted-foreground">
                          {t("settings.codexSshSync.sshAlias")}
                        </label>
                        <Input
                          value={host.sshAlias ?? ""}
                          onChange={(e) =>
                            updateHost(host.id, { sshAlias: e.target.value })
                          }
                          placeholder="my-codex-server"
                        />
                      </div>
                      <div className="space-y-1 sm:col-span-2">
                        <label className="text-xs text-muted-foreground">
                          {t("settings.codexSshSync.identityFile")}
                        </label>
                        <Input
                          value={host.identityFile ?? ""}
                          onChange={(e) =>
                            updateHost(host.id, { identityFile: e.target.value })
                          }
                          placeholder="D:\\sshkey\\id_ed25519"
                        />
                      </div>
                      <div className="space-y-1 sm:col-span-2">
                        <label className="text-xs text-muted-foreground">
                          {t("settings.codexSshSync.remoteCodexDir")}
                        </label>
                        <Input
                          value={host.remoteCodexDir ?? "~/.codex"}
                          onChange={(e) =>
                            updateHost(host.id, {
                              remoteCodexDir: e.target.value,
                            })
                          }
                        />
                      </div>
                    </div>

                    <div className="flex flex-col gap-2 text-xs">
                      <label className="flex items-center justify-between gap-3">
                        <span>{t("settings.codexSshSync.autoSync")}</span>
                        <Switch
                          checked={host.autoSync ?? true}
                          onCheckedChange={(v) =>
                            updateHost(host.id, { autoSync: v })
                          }
                        />
                      </label>
                      <label className="flex items-center justify-between gap-3">
                        <span>{t("settings.codexSshSync.syncOnConnect")}</span>
                        <Switch
                          checked={host.syncOnSshConnect ?? true}
                          onCheckedChange={(v) =>
                            updateHost(host.id, { syncOnSshConnect: v })
                          }
                        />
                      </label>
                      <label className="flex items-center justify-between gap-3">
                        <span>{t("settings.codexSshSync.forwardProxy")}</span>
                        <Switch
                          checked={host.forwardProxy ?? true}
                          onCheckedChange={(v) =>
                            updateHost(host.id, { forwardProxy: v })
                          }
                        />
                      </label>
                    </div>

                    {(host.lastError || host.lastSyncAt) && (
                      <p className="text-xs text-muted-foreground">
                        {host.lastError
                          ? t("settings.codexSshSync.lastError", {
                              error: host.lastError,
                            })
                          : t("settings.codexSshSync.lastSync", {
                              time: new Date(
                                host.lastSyncAt ?? 0,
                              ).toLocaleString(),
                            })}
                      </p>
                    )}

                    <div className="flex flex-wrap gap-2">
                      <Button
                        type="button"
                        size="sm"
                        variant="outline"
                        disabled={testingId === host.id}
                        onClick={() => handleTest(host)}
                      >
                        {testingId === host.id ? (
                          <Loader2 className="h-3.5 w-3.5 animate-spin" />
                        ) : (
                          <Cable className="h-3.5 w-3.5" />
                        )}
                        <span className="ml-1.5">
                          {t("settings.codexSshSync.test")}
                        </span>
                      </Button>
                      <Button
                        type="button"
                        size="sm"
                        variant="outline"
                        disabled={syncing}
                        onClick={() => handleSyncNow(host.id)}
                      >
                        {syncing ? (
                          <Loader2 className="h-3.5 w-3.5 animate-spin" />
                        ) : (
                          <UploadCloud className="h-3.5 w-3.5" />
                        )}
                        <span className="ml-1.5">
                          {t("settings.codexSshSync.syncOne")}
                        </span>
                      </Button>
                    </div>
                  </div>
                ) : null}
              </div>
            );
          })}

          <div className="flex flex-wrap gap-2">
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={handleAddHost}
            >
              <Plus className="h-3.5 w-3.5 mr-1.5" />
              {t("settings.codexSshSync.addHost")}
            </Button>
            <Button
              type="button"
              size="sm"
              disabled={saving}
              onClick={() => void handleSave()}
            >
              {saving ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin mr-1.5" />
              ) : (
                <Save className="h-3.5 w-3.5 mr-1.5" />
              )}
              {t("settings.codexSshSync.save")}
            </Button>
            <Button
              type="button"
              size="sm"
              variant="secondary"
              disabled={syncing || hosts.length === 0}
              onClick={() => void handleSyncNow()}
            >
              {syncing ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin mr-1.5" />
              ) : (
                <RefreshCw className="h-3.5 w-3.5 mr-1.5" />
              )}
              {t("settings.codexSshSync.syncAll")}
            </Button>
          </div>
        </div>
      ) : null}
    </section>
  );
}
