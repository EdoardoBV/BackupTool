import type { ReactNode } from "react";
import type { Tone } from "./session";

/** Small line icons, drawn on a 16-unit grid and coloured by the text around them. */
function Icon({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <svg className={className ? `icon ${className}` : "icon"} viewBox="0 0 16 16" aria-hidden="true">
      {children}
    </svg>
  );
}

export const CardIcon = () => (
  <Icon>
    <path d="M4.5 1.5h5l3 3v9a1 1 0 0 1-1 1h-7a1 1 0 0 1-1-1v-11a1 1 0 0 1 1-1z" />
    <path d="M6 4v2M8 4v2M10 5v1" />
  </Icon>
);

export const DriveIcon = () => (
  <Icon>
    <rect x="1.5" y="4" width="13" height="8" rx="2" />
    <path d="M4 9.5h.01M6.5 9.5h3" />
  </Icon>
);

export const NetworkIcon = () => (
  <Icon>
    <rect x="2.5" y="2" width="11" height="6.5" rx="1.5" />
    <path d="M5 5.25h.01M8 8.5v3.5M2.5 12.5h11" />
  </Icon>
);

export const FolderIcon =({ className }: { className?: string }) => (
  <Icon className={className}>
    <path d="M1.5 4.5a1 1 0 0 1 1-1h3l1.5 1.5h6.5a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1h-11a1 1 0 0 1-1-1z" />
  </Icon>
);

export const CameraIcon = () => (
  <Icon>
    <path d="M1.5 5.5a1 1 0 0 1 1-1h2l1-1.5h5l1 1.5h2a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1h-11a1 1 0 0 1-1-1z" />
    <circle cx="8" cy="8.3" r="2.3" />
  </Icon>
);

export const EjectIcon = () => (
  <Icon>
    <path d="M8 3l5 6H3z" />
    <path d="M3.5 12.5h9" />
  </Icon>
);

export const PlusIcon = () => (
  <Icon>
    <path d="M8 3.5v9M3.5 8h9" />
  </Icon>
);

export const CloseIcon = () => (
  <Icon>
    <path d="M4.5 4.5l7 7M11.5 4.5l-7 7" />
  </Icon>
);

/**
 * The state indicator: a lens seen from the front. The colour of its coating
 * says whether the card is safe; while a copy runs a reflection travels
 * round it.
 */
export function Lens({ tone, large, busy }: { tone: Tone; large?: boolean; busy?: boolean }) {
  const classes = ["lens", `tone-${tone}`, large && "lens-large", busy && "lens-busy"];
  return <span className={classes.filter(Boolean).join(" ")} aria-hidden="true" />;
}
