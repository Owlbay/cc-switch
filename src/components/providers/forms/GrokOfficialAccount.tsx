import React from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { AlertTriangle, Plus } from "lucide-react";
import { authApi } from "@/lib/api";
import type { ManagedAuthAccount } from "@/lib/api/auth";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectSeparator,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

const DEFAULT_LOGIN = "__default__";
const MANAGE_ACCOUNTS = "__manage_accounts__";

function useXaiAccounts() {
  return useQuery({
    queryKey: ["managed-auth-status", "xai_oauth"],
    queryFn: () => authApi.authGetStatus("xai_oauth"),
    staleTime: 30000,
  });
}

interface GrokAccountSelectProps {
  value: string | null;
  onChange: (accountId: string | null, account?: ManagedAuthAccount) => void;
  /** 打开授权中心添加或管理 xAI 账号 */
  onManageAccounts?: () => void;
}

/**
 * Grok 官方卡的登录方式，和 Claude / Codex 官方卡同一套写法：授权中心的 xAI 账号在上，
 * 「添加或管理」打开授权中心，最下面是跟随 Grok CLI 登录（~/.grok/auth.json，不绑号）。
 */
export function GrokAccountSelect({
  value,
  onChange,
  onManageAccounts,
}: GrokAccountSelectProps) {
  const { t } = useTranslation();
  const { data } = useXaiAccounts();
  const accounts = data?.accounts ?? [];
  const selected = accounts.find((a) => a.id === value);
  // 绑的号不在这台电脑上（别的设备同步来的、或已删掉）时也要显示出来，别悄悄改成跟随登录
  const missing = value !== null && !selected;
  const followLabel = t("provider.form.grokAccount.default", {
    defaultValue: "跟随 Grok CLI 登录",
  });
  const missingLabel = t("provider.form.grokAccount.missing", {
    defaultValue: "不在这台电脑上的账号",
  });
  const label = t("provider.form.grokAccount.label", {
    defaultValue: "登录方式",
  });
  const itemClass =
    "min-w-0 overflow-hidden py-2 pl-6 [&>span:last-child]:min-w-0 [&>span:last-child]:flex-1 [&>span:last-child]:overflow-hidden";

  return (
    <div className="space-y-2.5">
      <Label className="text-sm font-medium text-fg-1">{label}</Label>
      <Select
        value={value ?? DEFAULT_LOGIN}
        onValueChange={(next) => {
          // Radix Select 在表单里可能回报空串，忽略
          if (!next) return;
          if (next === MANAGE_ACCOUNTS) {
            onManageAccounts?.();
            return;
          }
          if (next === DEFAULT_LOGIN) {
            onChange(null);
            return;
          }
          onChange(
            next,
            accounts.find((a) => a.id === next),
          );
        }}
      >
        <SelectTrigger
          className="h-10 min-w-0 rounded-lg bg-surface px-3 shadow-sm"
          aria-label={label}
        >
          <span
            className="min-w-0 flex-1 truncate text-left text-sm font-medium tracking-tight"
            title={selected?.login}
          >
            <SelectValue>
              {/* 加载中也给字符串：children 为 undefined 时 Radix 会把选项文字
                  portal 进来，之后换成文字会和 React 抢着删节点 */}
              {value === null
                ? followLabel
                : (selected?.login ?? (data ? missingLabel : ""))}
            </SelectValue>
          </span>
        </SelectTrigger>
        <SelectContent className="w-[var(--radix-select-trigger-width)] max-w-[var(--radix-select-content-available-width)]">
          {accounts.map((account, index) => (
            <React.Fragment key={account.id}>
              <SelectItem
                value={account.id}
                disabled={account.requires_reauth}
                className={itemClass}
              >
                <div className="flex min-w-0 items-center gap-2">
                  <span aria-hidden className="h-4 w-4 shrink-0" />
                  <span
                    className="min-w-0 truncate text-sm font-medium leading-5"
                    title={account.login}
                  >
                    {account.login}
                  </span>
                  {account.requires_reauth && (
                    <span className="ml-1 inline-flex shrink-0 items-center gap-1 text-xs text-warning-text">
                      <AlertTriangle className="h-3 w-3" />
                      {t("provider.form.grokAccount.reauthBadge", {
                        defaultValue: "需要重新登录",
                      })}
                    </span>
                  )}
                </div>
              </SelectItem>
              {(index < accounts.length - 1 || onManageAccounts) && (
                <SelectSeparator className="mx-2 my-0 bg-border/60" />
              )}
            </React.Fragment>
          ))}
          {missing && (
            <SelectItem value={value} className={itemClass}>
              <div className="flex min-w-0 items-center gap-2">
                <span aria-hidden className="h-4 w-4 shrink-0" />
                <span className="min-w-0 truncate text-sm font-medium leading-5 text-warning-text">
                  {missingLabel}
                </span>
              </div>
            </SelectItem>
          )}
          {onManageAccounts && (
            <SelectItem value={MANAGE_ACCOUNTS} className="py-2 pl-6">
              <div className="flex items-center gap-2">
                <Plus className="h-4 w-4 shrink-0 text-fg-2" />
                <span className="truncate text-sm font-medium leading-5">
                  {t("provider.form.grokAccount.manage", {
                    defaultValue: "添加或管理 xAI 账号",
                  })}
                </span>
              </div>
            </SelectItem>
          )}
          {(accounts.length > 0 || missing || onManageAccounts) && (
            <SelectSeparator className="my-1.5 bg-border" />
          )}
          <SelectItem value={DEFAULT_LOGIN} className={itemClass}>
            <div className="flex min-w-0 items-center gap-2">
              <span aria-hidden className="h-4 w-4 shrink-0" />
              <span className="shrink-0 text-sm font-medium leading-5">
                {followLabel}
              </span>
              <span className="min-w-0 truncate text-sm leading-5 text-fg-2">
                {t("provider.form.grokAccount.defaultDescription", {
                  defaultValue: "账号会随 Grok CLI 当前登录变化",
                })}
              </span>
            </div>
          </SelectItem>
        </SelectContent>
      </Select>
      <p className="text-caption text-fg-2">
        {t("provider.form.grokAccount.help", {
          defaultValue:
            "切到这张卡后，Grok CLI 的登录（~/.grok/auth.json）会换成选中的账号；原来登录的号会先收进授权中心，不会丢。",
        })}
      </p>
    </div>
  );
}

/**
 * 绑了账号的 Grok 官方卡副标题（官网地址的位置）：写账号，有备注写备注，卡名里已经有账号
 * 就写「xAI 账号」；账号失效或不在这台电脑上时跟一句提示。
 */
export function GrokBoundAccountLine({
  accountId,
  providerName,
  note,
}: {
  accountId: string;
  providerName: string;
  note?: string;
}) {
  const { t } = useTranslation();
  const { data, isSuccess } = useXaiAccounts();
  if (!isSuccess) return null;
  const account = data?.accounts.find((a) => a.id === accountId);
  if (!account) {
    return (
      <div className="mt-0.5 flex min-w-0 items-center gap-1 text-caption text-warning-text">
        <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
        <span className="min-w-0 truncate">
          {t("provider.form.grokAccount.missing", {
            defaultValue: "不在这台电脑上的账号",
          })}
        </span>
      </div>
    );
  }
  const name = providerName.trim();
  const label =
    note ??
    (name === account.login || name.includes(account.login)
      ? t("provider.form.grokAccount.xaiAccount", {
          defaultValue: "xAI 账号",
        })
      : account.login);
  return (
    <div className="mt-0.5 flex min-w-0 items-center gap-2 text-caption text-fg-2">
      <span className="min-w-0 truncate" title={note ?? account.login}>
        {label}
      </span>
      {account.requires_reauth && (
        <span className="inline-flex shrink-0 items-center gap-1 text-warning-text">
          <AlertTriangle className="h-3.5 w-3.5" />
          {t("provider.form.grokAccount.reauthBadge", {
            defaultValue: "需要重新登录",
          })}
        </span>
      )}
    </div>
  );
}
