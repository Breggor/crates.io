import Ember from 'ember';
import ajax from 'ic-ajax';

export default Ember.ObjectController.extend({
    isLoading: false,
    actions: {
        download: function(version) {
            this.set('isLoading', true);
            var self = this;
            var crate_downloads = this.get('model').get('downloads');
            var ver_downloads = version.get('downloads');
            return ajax({
                url: version.get('dl_path'),
                dataType: 'json',
            }).then(function(data) {
                self.get('model').set('downloads', crate_downloads + 1);
                version.set('downloads', ver_downloads + 1);
                Ember.$('#download-frame').attr('src', data.url);
            }).finally(function() {
                self.set('isLoading', false);
            });
        }
    }
});

