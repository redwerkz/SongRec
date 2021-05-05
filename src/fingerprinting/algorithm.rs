
use chfft::RFft1D;
use std::error::Error;
use std::io::BufReader;
use std::process::Command;
use std::collections::HashMap;

use crate::fingerprinting::hanning::HANNING_WINDOW_2048_MULTIPLIERS;
use crate::fingerprinting::signature_format::{DecodedSignature, FrequencyBand, FrequencyPeak};

#[cfg(windows)]
use std::os::windows::process::CommandExt;


pub struct SignatureGenerator {
    
    // Used when processing input:
    
    ring_buffer_of_samples: [i16; 2048], /// Ring buffer.
    ring_buffer_of_samples_index: usize,
    
    reordered_ring_buffer_of_samples: [f32; 2048], /// Reordered, temporary version of the ring buffer above, with floats for precision because we applied Hanning window.
    
    fft_outputs: [[f32; 1025]; 256], /// Ring buffer. Lists of 1025 floats, premultiplied with a Hanning function before being passed through FFT, computed from the ring buffer every new 128 samples
    fft_outputs_index: usize,
    
    fft_object: RFft1D::<f32>,
    
    spread_fft_outputs: [[f32; 1025]; 256], /// Ring buffer.
    spread_fft_outputs_index: usize,
    
    num_spread_ffts_done: u32,
    
    signature: DecodedSignature
    
    
}

impl SignatureGenerator {
    
    pub fn make_signature_from_file(file_path: &str) -> Result<DecodedSignature, Box<dyn Error>> {
        
        // Decode the .WAV, .MP3, .OGG or .FLAC file
        
        let mut decoder = rodio::Decoder::new(BufReader::new(std::fs::File::open(file_path)?));
        
        if let Err(ref _decoding_error) = decoder {
            
            // Find the path for FFMpeg, in the case where it is installed
            
            let mut possible_ffmpeg_paths: Vec<&str> = vec!["ffmpeg", "ffmpeg.exe"];
            
            let mut current_dir_ffmpeg_path = std::env::current_exe()?;
            current_dir_ffmpeg_path.pop();
            current_dir_ffmpeg_path.push("ffmpeg.exe");
            
            possible_ffmpeg_paths.push(current_dir_ffmpeg_path.to_str().unwrap());
            
            let mut actual_ffmpeg_path: Option<&str> = None;
            
            for possible_path in possible_ffmpeg_paths {
                
                // Use .output() to execute the subprocess testing for FFMpeg
                // presence and correct execution, so that it does not pollute
                // the standard or error output in any way
                
                let mut command = Command::new(possible_path);
                let command = command.arg("-version");
                
                #[cfg(windows)]
                let command = command.creation_flags(0x00000008); // Set "CREATE_NO_WINDOW" on Windows
                
                if let Ok(process) = command.output() {
                    if process.status.success() {
                        actual_ffmpeg_path = Some(possible_path);
                        break;
                    }
                }
                
            }
            
            // If FFMpeg is available, use it to convert the input file
            // from whichever format to a .WAV (because Rodio has its
            // decoding support limited to .WAV, .FLAC, .OGG, .MP3, which
            // makes that .MP4/.AAC, .OPUS or .WMA are not supported, and
            // Rodio's minimp3 .MP3 decoder seems to crash on Windows anyway)
            
            if let Some(ffmpeg_path) = actual_ffmpeg_path {
                
                // Create a sink file for FFMpeg
                
                let sink_file = tempfile::Builder::new().suffix(".wav").tempfile()?;
                
                let sink_file_path = sink_file.into_temp_path();
                
                // Try to convert the input video or audio file to a standard
                // .WAV s16le PCM file using FFMpeg, and pass it to Rodio
                // later in the case where it succeeded
                
                let mut command = Command::new(ffmpeg_path);
                
                let command = command.args(&["-y", "-i", file_path,
                    sink_file_path.to_str().unwrap()]);
                
                // Set "CREATE_NO_WINDOW" on Windows, see
                // https://stackoverflow.com/a/60958956/662399
                #[cfg(windows)]
                let command = command.creation_flags(0x00000008);
                
                if let Ok(process) = command.output() {
                    
                    if process.status.success() {
                        decoder = rodio::Decoder::new(
                            BufReader::new(
                                std::fs::File::open(
                                    sink_file_path.to_str().unwrap()
                                )?
                            )
                        );
                    }
                }
                
            }
        }
        
        // Downsample the raw PCM samples to 16 KHz, and skip to the middle of the file
        // in order to increase recognition odds. Take 12 seconds of sample.
        
        let converted_file = rodio::source::UniformSourceIterator::new(decoder?, 1, 16000);
        
        let raw_pcm_samples: Vec<i16> = converted_file.collect();
        let mut raw_pcm_samples_slice: &[i16] = &raw_pcm_samples;
        
        let slice_len = raw_pcm_samples_slice.len().min(12 * 16000);
        
        if raw_pcm_samples_slice.len() > 12 * 16000 {
            let middle = raw_pcm_samples.len() / 2;
            
            raw_pcm_samples_slice = &raw_pcm_samples_slice[middle - (6 * 16000) .. middle + (6 * 16000)];
        }

        Ok(SignatureGenerator::make_signature_from_buffer(&raw_pcm_samples_slice[..slice_len]))

    }
    
    pub fn make_signature_from_buffer(s16_mono_16khz_buffer: &[i16]) -> DecodedSignature  {
        
        let mut this = SignatureGenerator {
            
            ring_buffer_of_samples: [0; 2048],
            ring_buffer_of_samples_index: 0,
            
            reordered_ring_buffer_of_samples: [0.0; 2048],
            
            fft_outputs: [[0.0; 1025]; 256],
            fft_outputs_index: 0,
            
            fft_object: RFft1D::<f32>::new(2048),
            
            spread_fft_outputs: [[0.0; 1025]; 256],
            spread_fft_outputs_index: 0,
            
            num_spread_ffts_done: 0,
            
            signature: DecodedSignature {
                sample_rate_hz: 16000,
                number_samples: s16_mono_16khz_buffer.len() as u32,
                frequency_band_to_sound_peaks: HashMap::new()
            }
            
        };
        
        for chunk in s16_mono_16khz_buffer.chunks_exact(128) {
            
            this.do_fft(chunk);
            
            this.do_peak_spreading();
            
            this.num_spread_ffts_done += 1;
            
            if this.num_spread_ffts_done >= 46 {
                this.do_peak_recognition();
            }
            
        }
        
        this.signature

    }
    
    fn do_fft(self: &mut Self, s16_mono_16khz_buffer: &[i16]) {
        
        // Copy the 128 input s16le samples to the local ring buffer
        
        self.ring_buffer_of_samples[self.ring_buffer_of_samples_index .. self.ring_buffer_of_samples_index + 128].copy_from_slice(s16_mono_16khz_buffer);
        
        self.ring_buffer_of_samples_index += 128;
        self.ring_buffer_of_samples_index &= 2047;
        
        // Reorder the items (put the latest data at end) and apply Hanning window
        
        for index in 0..=2047 {
            self.reordered_ring_buffer_of_samples[index] =
                self.ring_buffer_of_samples[(index + self.ring_buffer_of_samples_index) & 2047] as f32 *
                HANNING_WINDOW_2048_MULTIPLIERS[index];
        }
        
        // Perform Fast Fourier transform
        
        let complex_fft_results = self.fft_object.forward(&self.reordered_ring_buffer_of_samples);
        
        assert_eq!(complex_fft_results.len(), 1025);
        
        // Turn complex into reals, and put the results into a local array
        
        let real_fft_results = &mut self.fft_outputs[self.fft_outputs_index];
        
        for index in 0..=1024 {
            real_fft_results[index] = (
                (
                    complex_fft_results[index].re.powi(2) +
                    complex_fft_results[index].im.powi(2)
                ) / ((1 << 17) as f32)
            ).max(0.0000000001);
        }
        
        self.fft_outputs_index += 1;
        self.fft_outputs_index &= 255;
        
    }
    
    fn do_peak_spreading(self: &mut Self) {
        
        let real_fft_results = &self.fft_outputs[((self.fft_outputs_index as i32 - 1) & 255) as usize];
        
        let spread_fft_results = &mut self.spread_fft_outputs[self.spread_fft_outputs_index];
        
        // Perform frequency-domain spreading of peak values
        
        spread_fft_results.copy_from_slice(real_fft_results);
        
        for position in 0..=1022 {
            
            spread_fft_results[position] = spread_fft_results[position]
                .max(spread_fft_results[position + 1])
                .max(spread_fft_results[position + 2]);
            
        }
        
        // Perform time-domain spreading of peak values
        
        let spread_fft_results_copy = *spread_fft_results; // Avoid mutable+mutable borrow of self.spread_fft_outputs
        
        for position in 0..=1024 {
            
            for former_fft_number in &[1, 3, 6] {
                let former_fft_output = &mut self.spread_fft_outputs[((self.spread_fft_outputs_index as i32 - *former_fft_number) & 255) as usize];
                
                former_fft_output[position] = former_fft_output[position]
                    .max(spread_fft_results_copy[position]);
            }
        }
        
        self.spread_fft_outputs_index += 1;
        self.spread_fft_outputs_index &= 255;
        
    }
    
    fn do_peak_recognition(self: &mut Self) {
        
        // Note: when substracting an array index, casting to signed is needed
        // to avoid underflow panics at runtime.
        
        let fft_minus_46 = &self.fft_outputs[((self.fft_outputs_index as i32 - 46) & 255) as usize];
        let fft_minus_49 = &self.spread_fft_outputs[((self.spread_fft_outputs_index as i32 - 49) & 255) as usize];

        for bin_position in 10..=1014 {
            
            // Ensure that the bin is large enough to be a peak
            
            if fft_minus_46[bin_position] >= 1.0 / 64.0 &&
               fft_minus_46[bin_position] >= fft_minus_49[bin_position - 1] {
                   
                // Ensure that it is frequency-domain local minimum
                
                let mut max_neighbor_in_fft_minus_49: f32 = 0.0;
                
                for neighbor_offset in &[-10, -7, -4, -3, 1, 2, 5, 8] {
                    
                    max_neighbor_in_fft_minus_49 = max_neighbor_in_fft_minus_49
                        .max(fft_minus_49[(bin_position as i32 + *neighbor_offset) as usize]);
                      
                }
                
                if fft_minus_46[bin_position] > max_neighbor_in_fft_minus_49 {
                    
                    // Ensure that it is a time-domain local minimum
                    
                    let mut max_neighbor_in_other_adjacent_ffts = max_neighbor_in_fft_minus_49;
                    
                    for other_offset in &[-53, -45,
                        165, 172, 179, 186, 193, 200,
                        214, 221, 228, 235, 242, 249] {
                        
                        let other_fft = &self.spread_fft_outputs[((self.spread_fft_outputs_index as i32 + other_offset) & 255) as usize];
                        
                        max_neighbor_in_other_adjacent_ffts = max_neighbor_in_other_adjacent_ffts
                            .max(other_fft[bin_position - 1]);
                        
                    }
                    
                    if fft_minus_46[bin_position] > max_neighbor_in_other_adjacent_ffts {
                        
                        // This is a peak, store the peak
                        
                        let fft_pass_number = self.num_spread_ffts_done - 46;
                        
                        let peak_magnitude: f32 = fft_minus_46[bin_position].ln().max(1.0 / 64.0) * 1477.3 + 6144.0;
                        let peak_magnitude_before: f32 = fft_minus_46[bin_position - 1].ln().max(1.0 / 64.0) * 1477.3 + 6144.0;
                        let peak_magnitude_after: f32 = fft_minus_46[bin_position + 1].ln().max(1.0 / 64.0) * 1477.3 + 6144.0;
                                                
                        let peak_variation_1: f32 = peak_magnitude * 2.0 - peak_magnitude_before - peak_magnitude_after;
                        let peak_variation_2: f32 = (peak_magnitude_after - peak_magnitude_before) * 32.0 / peak_variation_1;
                        
                        let corrected_peak_frequency_bin: u16 = bin_position as u16 * 64 + peak_variation_2 as u16;
                        
                        assert!(peak_variation_1 >= 0.0);
                        
                        // Convert back a FFT bin to a frequency, given a 16 KHz sample
                        // rate, 1024 useful bins and the multiplication by 64 made before
                        // storing the information
                        
                        let frequency_hz: f32 = corrected_peak_frequency_bin as f32 * (16000.0 / 2.0 / 1024.0 / 64.0);
                        
                        // Ignore peaks outside the 250 Hz-5.5 KHz range, store them into
                        // a lookup table that will be used to generate the binary fingerprint
                        // otherwise
                        
                        let frequency_band = match frequency_hz as i32 {
                            250..=519 => FrequencyBand::_250_520,
                            520..=1449 => FrequencyBand::_520_1450,
                            1450..=3499 => FrequencyBand::_1450_3500,
                            3500..=5500 => FrequencyBand::_3500_5500,
                            _ => { continue; }
                        };
                        
                        if !self.signature.frequency_band_to_sound_peaks.contains_key(&frequency_band) {
                            self.signature.frequency_band_to_sound_peaks.insert(frequency_band, vec![]);
                        }
                                                
                        self.signature.frequency_band_to_sound_peaks.get_mut(&frequency_band).unwrap().push(
                            FrequencyPeak {
                                fft_pass_number: fft_pass_number,
                                peak_magnitude: peak_magnitude as u16,
                                corrected_peak_frequency_bin: corrected_peak_frequency_bin,
                                sample_rate_hz: 16000
                            }
                        );
                        
                        
                    }
                    
                }
                
                
            }
        }
        
    }
    
}
