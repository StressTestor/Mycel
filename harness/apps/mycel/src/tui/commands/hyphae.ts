import { LLM_NOT_SET_MESSAGE, NO_ACTIVE_SESSION_MESSAGE } from '../constant/kimi-tui';
import { performModelSwitch } from './config';
import type { SlashCommandHost } from './dispatch';
import { handleSwarmCommand } from './swarm';

/**
 * Session-only Mycel orchestration profile: xhigh reasoning plus standing
 * (or one-shot) multi-agent authorization.
 *
 * Hyphae deliberately reuses SwarmMode rather than maintaining a second
 * executor or a second permission bypass.
 */
export async function handleHyphaeCommand(
  host: SlashCommandHost,
  args: string,
): Promise<void> {
  if (host.session === undefined) {
    host.showError(NO_ACTIVE_SESSION_MESSAGE);
    return;
  }

  const prompt = args.trim();
  const subcommand = prompt.toLowerCase();
  if (subcommand === 'off' || (prompt.length === 0 && host.state.appState.swarmMode)) {
    await handleSwarmCommand(host, 'off');
    host.showStatus('Hyphae is off. Thinking effort remains unchanged.');
    return;
  }

  if (host.state.appState.model.trim().length === 0) {
    host.showError(LLM_NOT_SET_MESSAGE);
    return;
  }

  const effortReady =
    host.state.appState.thinkingEffort === 'xhigh' ||
    (await performModelSwitch(host, host.state.appState.model, 'xhigh', false));
  if (!effortReady) return;

  if (subcommand === 'on' || prompt.length === 0) {
    await handleSwarmCommand(host, 'on');
    return;
  }

  await handleSwarmCommand(host, prompt);
}
