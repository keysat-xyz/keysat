// Small helpers used across actions and init.

import { randomBytes } from 'crypto'

/** Generate a hex string secret (default 32 bytes = 64 hex chars). */
export function generateSecret(bytes = 32): string {
  return randomBytes(bytes).toString('hex')
}

/** Thin wrapper around fetch that attaches the admin bearer token. */
export async function adminCall(
  baseUrl: string,
  adminKey: string,
  path: string,
  init: RequestInit = {},
): Promise<Response> {
  return fetch(`${baseUrl}${path}`, {
    ...init,
    headers: {
      ...(init.headers || {}),
      'content-type': 'application/json',
      authorization: `Bearer ${adminKey}`,
    },
  })
}

/** Resolve the in-container licensing API URL from inside action scripts. */
export const LICENSING_URL = 'http://localhost:8080'
