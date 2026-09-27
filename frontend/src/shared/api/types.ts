import type { components } from "./generated-v2";

type Schemas = components["schemas"];
export type ErrorEnvelope = Schemas["ErrorEnvelope"];
export type LoginRequest = Schemas["Credentials"];
export type SetupRequest = Schemas["Credentials"];
export type SetupStatus = Schemas["SetupStatus"];
export type SetupState = SetupStatus["state"];
export type Session = Schemas["Session"];
export type AuthSession = Schemas["AuthSession"];
