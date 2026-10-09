import { decodeJwtPayload, firstText, jwtClaim, jwtDisplayName, object, tokenFingerprint } from "./json.ts";
import type { ResourceImportFile } from "./resource.ts";

/**
 * 凭证 JSON 导入的通用解析:许多 CLI 工具把账号凭证存成
 * { accounts: [...] } 形状,字段拼写各不相同(snake_case/camelCase)。
 * 这里统一递归收集 access/refresh token,插件再按自己的形状落库。
 */
export type CredentialCandidate = {
  accessToken: string;
  refreshToken: string | null;
  /** 额外发现的账号标识(如 ChatGPT account_id);收集器总是写入,手工构造可省略。 */
  accountId?: string | null;
  /** 条目中的 id_token;插件可据此补充展示名等声明。 */
  idToken?: string | null;
  displayName: string | null;
};

const ACCESS_TOKEN_KEYS = ["access_token", "accessToken", "token", "key"];
const REFRESH_TOKEN_KEYS = ["refresh_token", "refreshToken"];
const ACCOUNT_ID_KEYS = ["account_id", "accountId", "chatgpt_account_id"];
const ID_TOKEN_KEYS = ["id_token", "idToken"];
const NAME_KEYS = ["email", "display_name", "displayName", "name"];

/**
 * 递归收集凭证条目:数组逐层展开;accounts/credentials/items 嵌套下钻;
 * disabled 条目跳过;token 字段优先读 tokens 子对象,再读条目本身。
 * envKey 允许凭证文件用环境变量名(如 OPENAI_API_KEY)存放 token。
 */
export function collectCredentials(
  value: unknown,
  output: CredentialCandidate[],
  envKey?: string,
): void {
  if (Array.isArray(value)) {
    for (const item of value) collectCredentials(item, output, envKey);
    return;
  }
  const item = object(value);
  if (!item || item.disabled === true) return;
  for (const key of ["accounts", "credentials", "items"]) {
    if (Array.isArray(item[key])) {
      collectCredentials(item[key], output, envKey);
      return;
    }
  }
  const tokens = object(item.tokens) ?? item;
  const accessToken = firstText(tokens, ACCESS_TOKEN_KEYS) ??
    firstText(item, envKey ? [...ACCESS_TOKEN_KEYS, envKey] : ACCESS_TOKEN_KEYS);
  if (!accessToken) return;
  output.push({
    accessToken,
    refreshToken: firstText(tokens, REFRESH_TOKEN_KEYS) ?? firstText(item, REFRESH_TOKEN_KEYS),
    accountId: firstText(tokens, ACCOUNT_ID_KEYS) ?? firstText(item, ACCOUNT_ID_KEYS),
    idToken: firstText(tokens, ID_TOKEN_KEYS) ?? firstText(item, ID_TOKEN_KEYS),
    displayName: firstText(item, NAME_KEYS) ?? firstText(tokens, NAME_KEYS),
  });
}

/** 逐文件解析凭证 JSON;单文件问题记入 warnings,不中断整次导入。 */
export function parseCredentialFiles(
  files: ResourceImportFile[],
  providerName: string,
  envKey?: string,
): { credentials: CredentialCandidate[]; warnings: string[] } {
  const credentials: CredentialCandidate[] = [];
  const warnings: string[] = [];
  for (const file of files) {
    let content: unknown;
    try {
      content = JSON.parse(file.content);
    } catch {
      warnings.push(`${file.name}: not valid JSON`);
      continue;
    }
    const found: CredentialCandidate[] = [];
    collectCredentials(content, found, envKey);
    if (found.length === 0) {
      warnings.push(`${file.name}: no ${providerName} access token found`);
      continue;
    }
    credentials.push(...found);
  }
  return { credentials, warnings };
}

/**
 * JWT 访问令牌的账号标识:去重键取 sub → email → 令牌指纹,
 * 展示名取 email → preferred_username → name;都没有时回退到
 * fallbackDisplayName,缺省用去重键本身。
 */
export async function jwtAccountIdentity(
  accessToken: string,
  keyPrefix: string,
  fallbackDisplayName?: string,
): Promise<{ key: string; displayName: string }> {
  const payload = decodeJwtPayload(accessToken);
  const identity = jwtClaim(payload, "sub") ??
    jwtClaim(payload, "email") ??
    await tokenFingerprint(accessToken);
  return {
    key: `${keyPrefix}:${identity}`,
    displayName: jwtDisplayName(accessToken) ?? fallbackDisplayName ?? identity,
  };
}
