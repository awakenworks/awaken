import { type ButtonHTMLAttributes, type HTMLAttributes, type ReactNode } from "react";
export type TabsActivationMode = "automatic" | "manual";
export type TabsOrientation = "horizontal" | "vertical";
export type TabsProps = HTMLAttributes<HTMLDivElement> & {
    readonly value: string;
    readonly onValueChange: (value: string) => void;
    readonly activationMode?: TabsActivationMode;
    readonly orientation?: TabsOrientation;
};
export declare function Tabs({ value, onValueChange, activationMode, orientation, className, children, ...props }: TabsProps): import("react/jsx-runtime").JSX.Element;
export declare function TabList({ className, ...props }: HTMLAttributes<HTMLDivElement>): import("react/jsx-runtime").JSX.Element;
export type TabProps = Omit<ButtonHTMLAttributes<HTMLButtonElement>, "value"> & {
    readonly value: string;
};
export declare function Tab({ value, className, disabled, onClick, onKeyDown, ...props }: TabProps): import("react/jsx-runtime").JSX.Element;
export type TabPanelProps = HTMLAttributes<HTMLDivElement> & {
    readonly value: string;
    readonly children?: ReactNode;
};
export declare function TabPanel({ value, className, ...props }: TabPanelProps): import("react/jsx-runtime").JSX.Element;
