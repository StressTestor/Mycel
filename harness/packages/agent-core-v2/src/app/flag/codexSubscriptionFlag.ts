/**
 * `flag` domain — registers the Codex subscription flag defined by auth.
 */

import {
  CODEX_SUBSCRIPTION_AUTH_FLAG_ENV,
  CODEX_SUBSCRIPTION_AUTH_FLAG_ID,
} from '#/app/auth/flag';

import { type FlagDefinitionInput, registerFlagDefinition } from './flagRegistry';

export const codexSubscriptionAuthFlag: FlagDefinitionInput = {
  id: CODEX_SUBSCRIPTION_AUTH_FLAG_ID,
  title: 'Codex subscription authentication',
  description:
    'Allow OpenAI Responses providers to reuse a ChatGPT subscription login through Codex app-server.',
  env: CODEX_SUBSCRIPTION_AUTH_FLAG_ENV,
  default: false,
  surface: 'core',
};

registerFlagDefinition(codexSubscriptionAuthFlag);
