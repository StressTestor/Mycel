import { readConfigFile, writeConfigFile } from '../../config';
import type { KimiConfig, OAuthRef } from '../../config';
import { ErrorCodes, KimiError } from '../../errors';
import {
  CODEX_SUBSCRIPTION_AUTH_FLAG_ID,
  FLAG_DEFINITIONS,
  FlagResolver,
} from '../../flags';
import type { OAuthTokenProviderResolver } from '../../session/provider-manager';
import {
  applyManagedKimiCodeConfig,
  applyManagedKimiCodeLogoutConfig,
  CODEX_SUBSCRIPTION_BASE_URL,
  createCodexSubscriptionTokenProvider,
  getCachedCodexSubscriptionAccessToken,
  isCodexSubscriptionBaseUrl,
  isCodexSubscriptionOAuthRef,
  KIMI_CODE_PROVIDER_NAME,
  KimiOAuthToolkit,
  resolveKimiCodeLoginAuth,
  resolveKimiCodeRuntimeAuth,
  type BearerTokenProvider,
  type KimiOAuthLoginOptions,
  type ManagedKimiConfigShape,
} from '@moonshot-ai/kimi-code-oauth';

import type { IEnvironmentService } from '../environment/environment';

type ServicesManagedConfig = KimiConfig & ManagedKimiConfigShape;

type ServicesAuthLoginOptions = Omit<KimiOAuthLoginOptions, 'provisionConfig'>;

interface ServicesAuthLoginResult {
  readonly providerName: string;
  readonly ok: true;
  readonly defaultModel: string;
  readonly defaultThinking: boolean;
  readonly configPath?: string | undefined;
}

interface ServicesAuthLogoutResult {
  readonly providerName: string;
  readonly ok: true;
}

export interface ServicesAuthFacade {
  login(
    providerName?: string | undefined,
    options?: ServicesAuthLoginOptions,
  ): Promise<ServicesAuthLoginResult>;
  logout(providerName?: string | undefined): Promise<ServicesAuthLogoutResult>;
  getCachedAccessToken(
    providerName?: string,
    oauthRef?: OAuthRef | undefined,
  ): Promise<string | undefined>;
  readonly resolveOAuthTokenProvider: OAuthTokenProviderResolver;
}

class ServicesManagedAuthFacade implements ServicesAuthFacade {
  private readonly toolkit: KimiOAuthToolkit<ServicesManagedConfig>;
  private readonly codexTokenProvider = createCodexSubscriptionTokenProvider();

  constructor(
    private readonly options: Pick<IEnvironmentService, 'homeDir' | 'configPath'>,
  ) {
    this.toolkit = new KimiOAuthToolkit<ServicesManagedConfig>({
      homeDir: options.homeDir,
      configAdapter: {
        configPath: options.configPath,
        read: () => readConfigFile(options.configPath) as ServicesManagedConfig,
        write: async (config) => {
          await writeConfigFile(options.configPath, config);
        },
        apply: applyManagedKimiCodeConfig,
        remove: applyManagedKimiCodeLogoutConfig,
      },
    });
  }

  async login(
    providerName: string | undefined = KIMI_CODE_PROVIDER_NAME,
    options: ServicesAuthLoginOptions = {},
  ): Promise<ServicesAuthLoginResult> {
    const auth = this.resolveManagedAuth(providerName);
    const loginAuth = resolveKimiCodeLoginAuth({
      configuredBaseUrl: auth.baseUrl,
      configuredOAuthRef: auth.oauthRef,
      requestedBaseUrl: options.baseUrl,
      requestedOAuthHost: options.oauthHost,
    });
    const result = await this.toolkit.login(providerName, {
      ...options,
      baseUrl: loginAuth.baseUrl,
      oauthHost: loginAuth.oauthHost,
      oauthRef: options.oauthRef ?? loginAuth.oauthRef,
      provisionConfig: true,
    });
    if (result.provision === undefined) {
      throw new Error('Kimi auth login did not provision model config.');
    }
    return {
      providerName: result.providerName,
      ok: true,
      defaultModel: result.provision.defaultModel,
      defaultThinking: result.provision.defaultThinking,
      configPath: result.provision.configPath,
    };
  }

  async logout(
    providerName?: string | undefined,
  ): Promise<ServicesAuthLogoutResult> {
    const result = await this.toolkit.logout(
      providerName,
      this.resolveRuntimeManagedAuth(providerName).oauthRef,
    );
    return {
      providerName: result.providerName,
      ok: result.ok,
    };
  }

  async getCachedAccessToken(
    providerName?: string,
    oauthRef?: OAuthRef | undefined,
  ): Promise<string | undefined> {
    const configuredRef = oauthRef ?? this.resolveManagedAuth(providerName).oauthRef;
    if (isCodexSubscriptionOAuthRef(configuredRef)) {
      this.assertCodexSubscriptionEnabled(providerName);
      return getCachedCodexSubscriptionAccessToken();
    }
    return this.toolkit.getCachedAccessToken(
      providerName,
      this.runtimeOAuthRef(providerName, configuredRef),
    );
  }

  readonly resolveOAuthTokenProvider = (
    providerName: string,
    oauthRef?: OAuthRef | undefined,
  ): BearerTokenProvider => {
    if (isCodexSubscriptionOAuthRef(oauthRef)) {
      this.assertCodexSubscriptionEnabled(providerName);
      return this.codexTokenProvider;
    }
    return this.toolkit.tokenProvider(
      providerName,
      this.runtimeOAuthRef(providerName, oauthRef),
    );
  };

  private assertCodexSubscriptionEnabled(providerName?: string): void {
    const config = readConfigFile(this.options.configPath);
    const name = providerName ?? KIMI_CODE_PROVIDER_NAME;
    const provider = config.providers[name];
    if (
      provider?.type !== 'openai_responses' ||
      !isCodexSubscriptionBaseUrl(provider.baseUrl)
    ) {
      throw new KimiError(
        ErrorCodes.CONFIG_INVALID,
        `Codex subscription authentication requires an openai_responses provider at ${CODEX_SUBSCRIPTION_BASE_URL}.`,
      );
    }
    if (
      new FlagResolver(process.env, FLAG_DEFINITIONS, config.experimental).enabled(
        CODEX_SUBSCRIPTION_AUTH_FLAG_ID,
      )
    ) {
      return;
    }
    throw new KimiError(
      ErrorCodes.CONFIG_INVALID,
      'Codex subscription authentication is experimental. Enable `codex_subscription_auth = true` under `[experimental]` or set `KIMI_CODE_EXPERIMENTAL_CODEX_SUBSCRIPTION_AUTH=1`.',
    );
  }

  private resolveManagedAuth(providerName?: string | undefined): {
    readonly oauthRef?: OAuthRef | undefined;
    readonly baseUrl?: string | undefined;
  } {
    const name = providerName ?? KIMI_CODE_PROVIDER_NAME;
    const config = readConfigFile(this.options.configPath);
    const provider = config.providers[name];
    return {
      oauthRef: provider?.oauth,
      baseUrl: provider?.baseUrl,
    };
  }

  private resolveRuntimeManagedAuth(providerName?: string | undefined): {
    readonly oauthRef: OAuthRef;
    readonly baseUrl?: string | undefined;
  } {
    const auth = this.resolveManagedAuth(providerName);
    return resolveKimiCodeRuntimeAuth({
      configuredBaseUrl: auth.baseUrl,
      configuredOAuthRef: auth.oauthRef,
    });
  }

  private runtimeOAuthRef(
    providerName: string | undefined,
    oauthRef?: OAuthRef | undefined,
  ): OAuthRef | undefined {
    if ((providerName ?? KIMI_CODE_PROVIDER_NAME) !== KIMI_CODE_PROVIDER_NAME) {
      return oauthRef;
    }
    const auth = this.resolveManagedAuth(providerName);
    return resolveKimiCodeRuntimeAuth({
      configuredBaseUrl: auth.baseUrl,
      configuredOAuthRef: oauthRef ?? auth.oauthRef,
    }).oauthRef;
  }
}

export function createManagedAuthFacade(
  env: Pick<IEnvironmentService, 'homeDir' | 'configPath'>,
): ServicesAuthFacade {
  return new ServicesManagedAuthFacade(env);
}
