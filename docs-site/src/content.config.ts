import { defineCollection } from 'astro:content';
import { docsSchema, i18nSchema } from '@astrojs/starlight/schema';
import { i18nLoader } from '@astrojs/starlight/loaders';
import { glob } from 'astro/loaders';

const docs = defineCollection({
  loader: glob({
    base: new URL('../../docs/', import.meta.url),
    pattern: '**/*.md',
    generateId: ({ entry }) => entry === 'README.md' ? 'index' : entry.replace(/\.md$/, ''),
  }),
  schema: docsSchema(),
});

const i18n = defineCollection({ loader: i18nLoader(), schema: i18nSchema() });

export const collections = { docs, i18n };
