import React from 'react';
import { AbsoluteFill, Img, staticFile } from 'remotion';
import type { Shot } from '../manifest-types';
import { TitleCard } from './TitleCard';

/**
 * Background-only shot: a title card or a crisp full-frame screenshot. All
 * motion — zoom/pan (camera), cursor travel, highlight, captions — is now driven
 * globally by the camera timeline and the overlay layers, so a shot no longer
 * carries its own transform. The screenshot fills the stage at 1:1 (capture and
 * output share aspect ratio); the CameraRig above applies any zoom/pan.
 */
export const ShotBackground: React.FC<{ shot: Shot }> = ({ shot }) => {
  if (shot.title) {
    return <TitleCard title={shot.title} subtitle={shot.subtitle ?? ''} link={shot.link} />;
  }
  return (
    <AbsoluteFill style={{ backgroundColor: '#020617', overflow: 'hidden' }}>
      {shot.image ? (
        <Img
          src={staticFile(shot.image)}
          style={{ width: '100%', height: '100%', objectFit: 'cover' }}
        />
      ) : null}
    </AbsoluteFill>
  );
};
