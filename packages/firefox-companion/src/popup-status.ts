import { PROTOCOL_VERSION } from "./protocol.js";

export type FingerprintOwner = "host" | "popup";
export type HumanizeStatus = "on" | "off" | "unknown";

export type PopupStatus = {
  paired: boolean;
  unpairedReason?: string;
  companionId?: string;
  profileId?: string;
  leaseCount: number;
  nativeConnected: boolean;
  fingerprint: {
    enabled: boolean;
    owner: FingerprintOwner;
    sessionId?: string;
    seedHex?: string;
  };
  humanize: HumanizeStatus;
  lastError?: { code: string; message: string };
  protocolVersion: number;
};

export type PopupStatusInput = {
  paired: boolean;
  unpairedReason?: string;
  companionId?: string;
  profileId?: string;
  leaseCount: number;
  nativeConnected: boolean;
  fingerprintEnabled: boolean;
  fingerprintOwner: FingerprintOwner;
  fingerprintSessionId?: string;
  fingerprintSessionSeed?: number;
  lastError?: { code: string; message: string };
  protocolVersion: number;
};

export function buildPopupStatus(input: PopupStatusInput): PopupStatus {
  const fingerprint: PopupStatus["fingerprint"] = {
    enabled: input.fingerprintEnabled,
    owner: input.fingerprintOwner,
  };
  if (input.fingerprintSessionId !== undefined) {
    fingerprint.sessionId = input.fingerprintSessionId;
  }
  if (input.fingerprintSessionSeed !== undefined) {
    fingerprint.seedHex = input.fingerprintSessionSeed.toString(16);
  }

  const status: PopupStatus = {
    paired: input.paired,
    leaseCount: input.leaseCount,
    nativeConnected: input.nativeConnected,
    fingerprint,
    humanize: "unknown",
    protocolVersion: input.protocolVersion,
  };
  if (!input.paired && input.unpairedReason !== undefined) {
    status.unpairedReason = input.unpairedReason;
  }
  if (input.paired) {
    if (input.companionId !== undefined) status.companionId = input.companionId;
    if (input.profileId !== undefined) status.profileId = input.profileId;
  }
  if (input.lastError !== undefined) status.lastError = input.lastError;
  return status;
}

export { PROTOCOL_VERSION as POPUP_STATUS_PROTOCOL_FALLBACK };
