import { createI18n } from 'vue-i18n';
import { messages } from './locales';
import { safeSetString, STORAGE_KEYS } from '../lib/storage';

export const availableLocales = [
  { code: 'en', label: 'English' },
] as const;

export type LocaleCode = (typeof availableLocales)[number]['code'];

export const i18n = createI18n({
  legacy: false,
  locale: 'en',
  fallbackLocale: 'en',
  messages,
});

export function setLocale(l: LocaleCode): void {
  i18n.global.locale.value = l;
  safeSetString(STORAGE_KEYS.locale, l);
}

export default i18n;
