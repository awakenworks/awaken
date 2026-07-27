import type { ReactNode } from "react";
export interface AuthoringHeaderProps {
    readonly backLabel: string;
    readonly backIcon?: ReactNode;
    readonly onBack: () => void;
    readonly identity: ReactNode;
    readonly name: string;
    readonly nameLabel: string;
    readonly onNameChange: (value: string) => void;
    readonly nameSize?: number;
    readonly placeholder?: string;
    readonly metadata?: ReactNode;
    readonly windowed: boolean;
    readonly onWindowedChange: (windowed: boolean) => void;
    readonly maximizeLabel: string;
    readonly maximizeIcon?: ReactNode;
    readonly restoreLabel: string;
    readonly restoreIcon?: ReactNode;
    readonly closeLabel: string;
    readonly closeIcon?: ReactNode;
    readonly actionButtonClassName?: string;
    readonly onClose: () => void;
    readonly children: ReactNode;
}
export declare function AuthoringHeader({ backLabel, backIcon, onBack, identity, name, nameLabel, onNameChange, nameSize, placeholder, metadata, windowed, onWindowedChange, maximizeLabel, maximizeIcon, restoreLabel, restoreIcon, closeLabel, closeIcon, actionButtonClassName, onClose, children, }: AuthoringHeaderProps): import("react/jsx-runtime").JSX.Element;
export interface AuthoringGuideStep<Key extends string> {
    readonly key: Key;
    readonly label: ReactNode;
    readonly complete: boolean;
}
export interface AuthoringGuideProps<Key extends string> {
    readonly label: string;
    readonly steps: ReadonlyArray<AuthoringGuideStep<Key>>;
    readonly onSelect: (key: Key) => void;
    readonly completeIcon?: ReactNode;
    readonly incompleteIcon?: ReactNode;
    readonly blocked?: ReactNode;
    readonly className?: string;
}
export declare function AuthoringGuide<Key extends string>({ label, steps, onSelect, completeIcon, incompleteIcon, blocked, className, }: AuthoringGuideProps<Key>): import("react/jsx-runtime").JSX.Element;
//# sourceMappingURL=authoring.d.ts.map