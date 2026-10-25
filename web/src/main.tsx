import '@atlaskit/css-reset';
import { setGlobalTheme } from '@atlaskit/tokens/set-global-theme';
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import './styles.css';

// Atlassian design tokens; follows the OS light/dark preference.
void setGlobalTheme({ colorMode: 'auto', light: 'light', dark: 'dark', spacing: 'spacing', typography: 'typography' });

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
