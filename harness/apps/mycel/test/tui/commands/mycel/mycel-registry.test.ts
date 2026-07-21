import { describe, expect, it } from 'vitest';

import {
  BUILTIN_SLASH_COMMANDS,
  findBuiltInSlashCommand,
  resolveSlashCommandAvailability,
  type KimiSlashCommand,
} from '#/tui/commands/index';
import { MYCEL_SLASH_COMMANDS } from '#/tui/commands/mycel/index';

const FAMILY = ['immunity', 'gate', 'substrate', 'candidates', 'promote', 'deny', 'delegate'];

describe('mycel command family registration', () => {
  it('registers all seven commands as always-available built-ins', () => {
    for (const name of FAMILY) {
      const command = findBuiltInSlashCommand(name);
      expect(command, name).toBeDefined();
      expect(resolveSlashCommandAvailability(command!, '')).toBe('always');
    }
  });

  it('resolves the family aliases', () => {
    expect(findBuiltInSlashCommand('antibodies')?.name).toBe('immunity');
    expect(findBuiltInSlashCommand('guard')?.name).toBe('gate');
    expect(findBuiltInSlashCommand('doorman')?.name).toBe('gate');
    expect(findBuiltInSlashCommand('marrow')?.name).toBe('substrate');
    expect(findBuiltInSlashCommand('learned')?.name).toBe('candidates');
    expect(findBuiltInSlashCommand('sign')?.name).toBe('promote');
    expect(findBuiltInSlashCommand('block')?.name).toBe('deny');
    expect(findBuiltInSlashCommand('handoff')?.name).toBe('delegate');
  });

  it('carries argument hints + completion for the action commands', () => {
    const promote = findBuiltInSlashCommand('promote') as KimiSlashCommand | undefined;
    expect(promote?.argumentHint).toBe('<id> [severity] [refusal-mode]');
    expect(typeof promote?.completeArgs).toBe('function');
    expect((findBuiltInSlashCommand('deny') as KimiSlashCommand | undefined)?.argumentHint).toBe(
      '<command-pattern>',
    );
    expect((findBuiltInSlashCommand('delegate') as KimiSlashCommand | undefined)?.argumentHint).toBe(
      '<task>',
    );
  });

  it('has no duplicate names or aliases across the whole registry', () => {
    const identifiers: string[] = [];
    for (const command of BUILTIN_SLASH_COMMANDS) {
      identifiers.push(command.name, ...command.aliases);
    }
    expect(new Set(identifiers).size).toBe(identifiers.length);
  });

  it('clusters the family at a shared priority', () => {
    const priorities = new Set(MYCEL_SLASH_COMMANDS.map((command) => command.priority));
    expect(priorities.size).toBe(1);
  });
});
